//! Connection profiles.
//!
//! A profile is everything needed to build an `Operator`: a name, an OpenDAL
//! URI, and the options — credentials included.
//!
//! **Credentials are stored in this file, in plaintext.** That is a deliberate
//! choice, not an oversight: the platform keychain prompted for permission on
//! every launch and made a saved connection feel unreliable. The file is created
//! `0600`, so it is readable only by its owner, and [`crate::service`] marks
//! which fields are secret so the form can mask them. What it is *not* is safe
//! to sync, commit, or paste into an issue — the previous design was, and this
//! one is not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::service;
use crate::{Error, Result};

pub type ProfileId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Stable identifier. Renaming the profile must not change it.
    pub id: ProfileId,
    pub name: String,
    /// An OpenDAL URI: `fs:///Users/me`, `s3://bucket/prefix`,
    /// `webdav://host/dav`, `gcs://bucket`, `azblob://container`.
    pub uri: String,
    /// Everything `Operator::from_uri` needs, credentials included.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Credential *names* written by an older version of this app, when values
    /// lived in the platform keychain. Kept only so the app can say why such a
    /// connection stopped working — the values are not reachable any more. See
    /// [`Self::orphaned_by_keychain_removal`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

    /// Reject a profile that cannot connect.
    ///
    /// Runs on every save and again at connect time. It no longer polices which
    /// keys may be stored — credentials belong in `options` now — so what is
    /// left is structural: an id, a usable URI, and whatever the service says it
    /// cannot do without.
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

        // A URI whose scheme we have no schema for is still allowed through:
        // OpenDAL supports more services than the form offers, and a profile
        // hand-written for one of them should keep working.
        if let Some(service) = service::for_scheme(self.scheme()) {
            for field in service.fields {
                if !field.required || field.role != service::Role::Option {
                    continue;
                }
                if !self.options.contains_key(field.key) {
                    return Err(Error::Config(format!(
                        "连接 \"{}\" 缺少{}",
                        self.name, field.label
                    )));
                }
            }
        }

        Ok(())
    }

    /// Options to hand to `Operator::from_uri`.
    pub fn connect_options(&self) -> Result<Vec<(String, String)>> {
        self.validate()?;

        Ok(self
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    /// A profile written when credentials lived in the keychain, whose values are
    /// therefore gone.
    ///
    /// Worth naming rather than letting the connection fail obscurely: the file
    /// still lists what it needs, the value simply is not there any more, and the
    /// only fix is to type it in again.
    pub fn orphaned_by_keychain_removal(&self) -> Vec<&str> {
        self.secrets
            .iter()
            .map(|s| s.as_str())
            .filter(|key| !self.options.contains_key(*key))
            .collect()
    }

    /// Which of this profile's options hold credentials, for redaction.
    ///
    /// Derived from the service schema rather than from key spelling, so the
    /// answer is the same one the form used when it decided what to mask.
    pub fn secret_keys(&self) -> Vec<&str> {
        let Some(service) = service::for_scheme(self.scheme()) else {
            return Vec::new();
        };
        service
            .fields
            .iter()
            .filter(|f| f.is_secret())
            .map(|f| f.key)
            .filter(|key| self.options.contains_key(*key))
            .collect()
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

        // Deliberately *not* validated here. A half-configured connection should
        // still appear in the sidebar and explain itself when clicked — the
        // previous behaviour returned `Err` for the whole file, so one profile
        // missing a field made every other connection disappear. Validation
        // happens at connect time, where it has somewhere to show the message.
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

    fn s3_profile() -> Profile {
        let mut profile = Profile::new("prod-s3", "Prod S3", "s3://my-bucket/data");
        profile
            .options
            .insert("region".into(), "ap-northeast-1".into());
        profile
            .options
            .insert("access_key_id".into(), "AKIA".into());
        profile
            .options
            .insert("secret_access_key".into(), "shhh".into());
        profile
    }

    #[test]
    fn scheme_comes_from_the_uri() {
        assert_eq!(s3_profile().scheme(), "s3");
        assert_eq!(Profile::new("l", "Local", "fs:///tmp").scheme(), "fs");
        assert_eq!(Profile::new("b", "Bad", "nonsense").scheme(), "");
    }

    #[test]
    fn a_credential_in_options_is_now_allowed() {
        // The inverse of what this file used to assert. Credentials live in
        // `options` now, so a profile carrying one must validate — the old guard
        // would have rejected exactly the profiles the form produces.
        let profile = s3_profile();
        assert!(profile.options.contains_key("secret_access_key"));
        profile.validate().unwrap();
    }

    #[test]
    fn secret_keys_come_from_the_schema_not_from_spelling() {
        let profile = s3_profile();
        let mut keys = profile.secret_keys();
        keys.sort();
        assert_eq!(keys, vec!["secret_access_key"]);

        // `access_key_id` reads like a credential but the schema calls it plain
        // text, and the schema is what the form masked by — so redaction has to
        // agree with it rather than guess from the name.
        assert!(!keys.contains(&"access_key_id"));
    }

    #[test]
    fn a_structurally_required_option_is_still_rejected() {
        // azblob cannot address anything without knowing the account, and that is
        // not a credential — it is part of the address.
        let profile = Profile::new("az", "Azure", "azblob://container/");
        let err = profile.validate().unwrap_err();
        assert!(
            err.user_message().contains("账户名"),
            "got {}",
            err.user_message()
        );
    }

    #[test]
    fn an_s3_profile_with_no_credentials_is_valid() {
        // Regression: requiring static keys broke a real profile named
        // "s3-iam-dev". S3 reads credentials from an IAM role, the instance
        // metadata service or the environment, so a profile carrying none is a
        // normal configuration — not an incomplete one.
        let mut profile = Profile::new("iam", "IAM dev", "s3://bucket/");
        // Region is required for a different reason — OpenDAL's builder cannot
        // proceed without it — so a realistic IAM profile has one and no keys.
        profile.options.insert("region".into(), "us-east-1".into());

        profile.validate().unwrap();
    }

    #[test]
    fn a_scheme_with_no_schema_still_validates() {
        // OpenDAL supports more services than the form offers; a hand-written
        // profile for one of them must not be rejected for lacking fields we
        // have no schema for.
        let profile = Profile::new("x", "Exotic", "ipfs://somecid");
        profile.validate().unwrap();
    }

    #[test]
    fn an_invalid_uri_is_rejected() {
        let profile = Profile::new("x", "X", "not-a-uri");
        assert!(profile.validate().is_err());
    }

    #[test]
    fn connect_options_hand_over_everything_including_credentials() {
        let profile = s3_profile();
        let mut opts = profile.connect_options().unwrap();
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
    fn a_profile_written_for_the_keychain_says_what_it_lost() {
        // What the user's own config looked like: credential *names* recorded,
        // values in the keychain we no longer read. The connection cannot work,
        // and the only honest thing is to name the fields that have to be typed
        // in again rather than fail with something obscure.
        let mut profile = Profile::new("old", "Prod S3", "s3://bucket/");
        profile.secrets = vec!["access_key_id".into(), "secret_access_key".into()];

        assert_eq!(
            profile.orphaned_by_keychain_removal(),
            vec!["access_key_id", "secret_access_key"]
        );

        // Once re-entered, nothing is orphaned any more.
        profile
            .options
            .insert("access_key_id".into(), "AKIA".into());
        profile
            .options
            .insert("secret_access_key".into(), "shhh".into());
        assert!(profile.orphaned_by_keychain_removal().is_empty());
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
    fn the_saved_file_does_contain_the_credential() {
        // Asserted deliberately, and it is the inverse of what this test used to
        // check. Credentials are in this file now; the protection is the file
        // mode below, not their absence. A test that pretended otherwise would
        // be the most misleading thing in the repo.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        ProfileStore::at(&path).save(&[s3_profile()]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("shhh"), "the credential should be here");
        assert!(text.contains("ap-northeast-1"));
    }

    #[test]
    fn save_accepts_a_profile_carrying_a_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        let mut profile = s3_profile();
        profile
            .options
            .insert("session_token".into(), "hunter2".into());

        ProfileStore::at(&path).save(&[profile]).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn one_unusable_profile_does_not_hide_the_others() {
        // Regression: `load` used to validate and return `Err` for the whole
        // file, so a single connection missing a field made *every* saved
        // connection disappear from the sidebar. Loading now reads what is there;
        // connecting is where a bad profile explains itself.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(
            &path,
            r#"
[[profile]]
id = "half"
name = "半个连接"
uri = "azblob://container/"

[[profile]]
id = "good"
name = "本机"
uri = "fs:///tmp"
"#,
        )
        .unwrap();

        let profiles = ProfileStore::at(&path).load().unwrap();
        assert_eq!(profiles.len(), 2, "both must survive loading");
        // And the broken one still refuses to connect, with a reason.
        assert!(profiles[0].validate().is_err());
        profiles[1].validate().unwrap();
    }

    #[test]
    fn a_legacy_secrets_list_survives_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(
            &path,
            r#"
[[profile]]
id = "s3-iam-dev"
name = "IAM dev"
uri = "s3://s3-iam-dev/"
secrets = ["access_key_id", "secret_access_key"]
"#,
        )
        .unwrap();

        let profiles = ProfileStore::at(&path).load().unwrap();
        assert_eq!(profiles[0].orphaned_by_keychain_removal().len(), 2);
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
