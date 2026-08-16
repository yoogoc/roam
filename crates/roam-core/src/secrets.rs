//! Credential storage.
//!
//! Secrets live in the platform keychain, never in `profiles.toml`. The trait
//! exists so tests can run against an in-memory store — exercising the real
//! keychain would prompt the user for permission and leave test junk in their
//! login keyring.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Error, Result};

/// Keychain service name. Entries are addressed as `service/account` where the
/// account is `<profile id>/<option key>`.
const SERVICE: &str = "dev.roam.credentials";

pub trait SecretStore: Send + Sync {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>>;
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()>;
    /// Deleting an entry that does not exist succeeds.
    fn delete(&self, profile: &str, key: &str) -> Result<()>;
}

fn account(profile: &str, key: &str) -> String {
    format!("{profile}/{key}")
}

fn secret_err(op: &str, e: keyring::Error) -> Error {
    Error::Secret(format!("{op} 凭据失败: {e}"))
}

/// The platform-native store: Keychain on macOS, Credential Manager on Windows,
/// Secret Service on Linux.
pub struct Keychain;

impl SecretStore for Keychain {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, &account(profile, key))
            .map_err(|e| secret_err("打开", e))?;

        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secret_err("读取", e)),
        }
    }

    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &account(profile, key))
            .map_err(|e| secret_err("打开", e))?;
        entry.set_password(value).map_err(|e| secret_err("写入", e))
    }

    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &account(profile, key))
            .map_err(|e| secret_err("打开", e))?;

        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secret_err("删除", e)),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&account(profile, key))
            .cloned())
    }

    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        self.map
            .lock()
            .unwrap()
            .insert(account(profile, key), value.to_string());
        Ok(())
    }

    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        self.map.lock().unwrap().remove(&account(profile, key));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_secret() {
        let store = MemorySecrets::new();

        assert_eq!(store.get("prod", "secret_access_key").unwrap(), None);
        store.set("prod", "secret_access_key", "s3cr3t").unwrap();
        assert_eq!(
            store.get("prod", "secret_access_key").unwrap().as_deref(),
            Some("s3cr3t")
        );
    }

    #[test]
    fn profiles_do_not_share_credentials() {
        let store = MemorySecrets::new();
        store.set("prod", "password", "a").unwrap();
        store.set("staging", "password", "b").unwrap();

        assert_eq!(store.get("prod", "password").unwrap().as_deref(), Some("a"));
        assert_eq!(
            store.get("staging", "password").unwrap().as_deref(),
            Some("b")
        );
    }

    #[test]
    fn deleting_a_missing_secret_is_not_an_error() {
        let store = MemorySecrets::new();
        assert!(store.delete("prod", "nothing").is_ok());
    }

    #[test]
    fn delete_removes_only_the_named_key() {
        let store = MemorySecrets::new();
        store.set("prod", "access_key_id", "id").unwrap();
        store.set("prod", "secret_access_key", "key").unwrap();

        store.delete("prod", "access_key_id").unwrap();

        assert_eq!(store.get("prod", "access_key_id").unwrap(), None);
        assert!(store.get("prod", "secret_access_key").unwrap().is_some());
    }
}
