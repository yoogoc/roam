//! Connection profiles.
//!
//! A profile is everything needed to build an `Operator` *except* the
//! credentials: a name, an OpenDAL URI, and non-secret options like region or
//! endpoint. Credentials are named in `secrets` and fetched from a
//! [`SecretStore`] at connect time, so `profiles.toml` stays safe to read,
//! sync, or paste into an issue.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::secrets::SecretStore;
use crate::{Error, Result};

pub type ProfileId = String;

/// Option keys that must never be written to disk.
///
/// Checked as a case-insensitive substring so that per-service spellings
/// (`azblob` uses `account_key`, GCS uses `credential`) are all caught by one
/// list rather than needing an entry per backend.
pub const SENSITIVE_FRAGMENTS: &[&str] = &[
    "secret",
    "password",
    "token",
    "credential",
    "access_key",
    "account_key",
    "api_key",
    "private_key",
];

pub fn is_sensitive(key: &str) -> bool {
    let key = key.to_lowercase();
    SENSITIVE_FRAGMENTS
        .iter()
        .any(|fragment| key.contains(fragment))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Stable identifier, also the keychain account prefix. Renaming the
    /// profile must not change it, or its credentials become unreachable.
    pub id: ProfileId,
    pub name: String,
    /// An OpenDAL URI: `fs:///Users/me`, `s3://bucket/prefix`,
    /// `webdav://host/dav`, `gcs://bucket`, `azblob://container`.
    pub uri: String,
    /// Non-secret connection options only.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Option keys whose values live in the secret store.
    #[serde(default)]
    pub secrets: Vec<String>,
}

impl Profile {
    pub fn new(id: impl Into<String>, name: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            uri: uri.into(),
            options: BTreeMap::new(),
            secrets: Vec::new(),
        }
    }

    /// The URI scheme, used to label the connection and pick an icon.
    pub fn scheme(&self) -> &str {
        self.uri.split_once("://").map(|(s, _)| s).unwrap_or("")
    }

    /// Reject anything that would put a credential on disk.
    ///
    /// This is the guard that makes the "no plaintext secrets in the config
    /// file" claim true rather than aspirational: it runs on every save, so a
    /// caller cannot quietly stuff a secret into `options`.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::Config("连接 id 不能为空".into()));
        }
        if self.uri.trim().is_empty() || !self.uri.contains("://") {
            return Err(Error::Config(format!(
                "连接 \"{}\" 的 URI 无效，需要形如 s3://bucket/prefix",
                self.name
            )));
        }

        for key in self.options.keys() {
            if is_sensitive(key) {
                return Err(Error::Config(format!(
                    "选项 \"{key}\" 看起来是凭据，不能写进配置文件；请把它列入 secrets"
                )));
            }
        }

        Ok(())
    }

    /// Options to hand to `Operator::from_uri`, with credentials merged in.
    ///
    /// A credential named in `secrets` but absent from the store is an error
    /// rather than an omission — connecting without it would fail later with a
    /// far less obvious message.
    pub fn connect_options(&self, store: &dyn SecretStore) -> Result<Vec<(String, String)>> {
        self.validate()?;

        let mut out: Vec<(String, String)> = self
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for key in &self.secrets {
            match store.get(&self.id, key)? {
                Some(value) => out.push((key.clone(), value)),
                None => {
                    return Err(Error::Config(format!(
                        "连接 \"{}\" 缺少凭据 \"{key}\"，请重新填写",
                        self.name
                    )));
                }
            }
        }

        Ok(out)
    }
}

/// Turn a display name into an id safe for a filename and a keychain account.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppresses a leading dash

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "connection".to_string()
    } else {
        slug
    }
}

/// A slug not already used by `taken`, suffixing `-2`, `-3`, … as needed.
///
/// Ids double as keychain account prefixes, so a collision would silently point
/// two profiles at one credential.
pub fn unique_id(name: &str, taken: &[ProfileId]) -> ProfileId {
    let base = slugify(name);
    if !taken.iter().any(|id| id == &base) {
        return base;
    }

    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.iter().any(|id| id == candidate))
        .expect("an unbounded range always yields a free id")
}

/// Parse `key = value` lines from a text field.
///
/// Blank lines and `#` comments are skipped so a pasted config block works.
pub fn parse_kv_lines(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();

    for (ix, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::Config(format!(
                "第 {} 行无法解析，需要 key = value 的形式：{line}",
                ix + 1
            )));
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(Error::Config(format!("第 {} 行缺少键名", ix + 1)));
        }

        out.insert(key.to_string(), value.trim().to_string());
    }

    Ok(out)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default, rename = "profile")]
    profiles: Vec<Profile>,
}

/// Reads and writes `profiles.toml`.
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/Library/Application Support/dev.roam.Roam/profiles.toml` on macOS and
    /// the platform equivalent elsewhere.
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "roam", "Roam")
            .ok_or_else(|| Error::Config("无法确定配置目录".into()))?;
        Ok(dirs.config_dir().join("profiles.toml"))
    }

    pub fn default_store() -> Result<Self> {
        Ok(Self::at(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing file is an empty profile list, not an error — that is simply a
    /// first run.
    pub fn load(&self) -> Result<Vec<Profile>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::Config(format!(
                    "读取 {} 失败: {e}",
                    self.path.display()
                )));
            }
        };

        let parsed: ProfilesFile = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("解析 {} 失败: {e}", self.path.display())))?;

        for profile in &parsed.profiles {
            profile.validate()?;
        }

        Ok(parsed.profiles)
    }

    pub fn save(&self, profiles: &[Profile]) -> Result<()> {
        for profile in profiles {
            profile.validate()?;
        }

        let text = toml::to_string_pretty(&ProfilesFile {
            profiles: profiles.to_vec(),
        })
        .map_err(|e| Error::Config(format!("序列化连接配置失败: {e}")))?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("创建 {} 失败: {e}", parent.display())))?;
        }

        std::fs::write(&self.path, text)
            .map_err(|e| Error::Config(format!("写入 {} 失败: {e}", self.path.display())))?;

        // The file holds no secrets, but it does describe someone's
        // infrastructure; keep it owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecrets;

    fn s3_profile() -> Profile {
        let mut profile = Profile::new("prod-s3", "Prod S3", "s3://my-bucket/data");
        profile
            .options
            .insert("region".into(), "ap-northeast-1".into());
        profile.secrets = vec!["access_key_id".into(), "secret_access_key".into()];
        profile
    }

    #[test]
    fn scheme_comes_from_the_uri() {
        assert_eq!(s3_profile().scheme(), "s3");
        assert_eq!(Profile::new("l", "Local", "fs:///tmp").scheme(), "fs");
        assert_eq!(Profile::new("b", "Bad", "nonsense").scheme(), "");
    }

    #[test]
    fn a_credential_in_options_is_rejected() {
        let mut profile = s3_profile();
        profile
            .options
            .insert("secret_access_key".into(), "oops".into());

        let err = profile.validate().unwrap_err();
        assert!(
            err.user_message().contains("secret_access_key"),
            "got {}",
            err.user_message()
        );
    }

    #[test]
    fn sensitive_detection_covers_per_service_spellings() {
        for key in [
            "secret_access_key",
            "SECRET_ACCESS_KEY",
            "password",
            "session_token",
            "credential",
            "account_key",
            "api_key",
            "private_key",
        ] {
            assert!(is_sensitive(key), "{key} should be treated as a credential");
        }

        for key in [
            "region",
            "endpoint",
            "root",
            "bucket",
            "container",
            "server",
        ] {
            assert!(!is_sensitive(key), "{key} is not a credential");
        }
    }

    #[test]
    fn an_invalid_uri_is_rejected() {
        let profile = Profile::new("x", "X", "not-a-uri");
        assert!(profile.validate().is_err());
    }

    #[test]
    fn connect_options_merge_secrets_in() {
        let profile = s3_profile();
        let store = MemorySecrets::new();
        store.set("prod-s3", "access_key_id", "AKIA").unwrap();
        store.set("prod-s3", "secret_access_key", "shhh").unwrap();

        let mut opts = profile.connect_options(&store).unwrap();
        opts.sort();

        assert_eq!(
            opts,
            vec![
                ("access_key_id".to_string(), "AKIA".to_string()),
                ("region".to_string(), "ap-northeast-1".to_string()),
                ("secret_access_key".to_string(), "shhh".to_string()),
            ]
        );
    }

    #[test]
    fn a_missing_credential_fails_loudly_at_connect_time() {
        let profile = s3_profile();
        let store = MemorySecrets::new();
        store.set("prod-s3", "access_key_id", "AKIA").unwrap();
        // secret_access_key was never stored.

        let err = profile.connect_options(&store).unwrap_err();
        assert!(
            err.user_message().contains("secret_access_key"),
            "got {}",
            err.user_message()
        );
    }

    #[test]
    fn slugify_produces_usable_ids() {
        assert_eq!(slugify("Prod S3"), "prod-s3");
        assert_eq!(slugify("生产 / 备份"), "connection", "no ascii to keep");
        assert_eq!(slugify("  weird__name!! "), "weird-name");
        assert_eq!(slugify(""), "connection");
    }

    #[test]
    fn unique_id_avoids_collisions() {
        let taken = vec!["prod-s3".to_string(), "prod-s3-2".to_string()];

        assert_eq!(unique_id("Staging", &taken), "staging");
        assert_eq!(unique_id("Prod S3", &taken), "prod-s3-3");
    }

    #[test]
    fn kv_lines_parse_with_comments_and_blanks() {
        let parsed = parse_kv_lines(
            "
            # region first
            region = ap-northeast-1

            endpoint=https://example.com
            ",
        )
        .unwrap();

        assert_eq!(parsed.get("region").unwrap(), "ap-northeast-1");
        assert_eq!(parsed.get("endpoint").unwrap(), "https://example.com");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn kv_lines_report_the_offending_line() {
        let err = parse_kv_lines("region = us-east-1\nthis is not a pair").unwrap_err();
        assert!(
            err.user_message().contains("第 2 行"),
            "got {}",
            err.user_message()
        );
    }

    #[test]
    fn kv_values_may_contain_equals_signs() {
        // Base64 and query strings routinely do.
        let parsed = parse_kv_lines("token_url = https://x/y?a=b&c=d").unwrap();
        assert_eq!(parsed.get("token_url").unwrap(), "https://x/y?a=b&c=d");
    }

    #[test]
    fn missing_config_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::at(dir.path().join("profiles.toml"));

        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn profiles_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::at(dir.path().join("nested/profiles.toml"));

        let profiles = vec![s3_profile(), Profile::new("local", "本机", "fs:///tmp")];
        store.save(&profiles).unwrap();

        assert_eq!(store.load().unwrap(), profiles);
    }

    #[test]
    fn the_saved_file_contains_no_secret_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let store = ProfileStore::at(&path);

        let secrets = MemorySecrets::new();
        secrets
            .set("prod-s3", "secret_access_key", "TOP-SECRET")
            .unwrap();
        store.save(&[s3_profile()]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("TOP-SECRET"), "secret leaked into {text}");
        // The key *name* is recorded so we know what to fetch; the value is not.
        assert!(text.contains("secret_access_key"));
        assert!(text.contains("ap-northeast-1"));
    }

    #[test]
    fn save_refuses_a_profile_carrying_a_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let store = ProfileStore::at(&path);

        let mut bad = s3_profile();
        bad.options.insert("password".into(), "hunter2".into());

        assert!(store.save(&[bad]).is_err());
        assert!(
            !path.exists(),
            "nothing should be written when validation fails"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        ProfileStore::at(&path).save(&[s3_profile()]).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
