//! What each backend actually needs, as data.
//!
//! One table drives three things that used to be able to disagree: the fields
//! the connection form renders, the validation that runs on save, and the
//! `Operator` options handed to OpenDAL. Before this the user typed
//! `key = value` lines and only found out at connect time whether the keys were
//! the ones the service wanted.
//!
//! The option names here are not invented — every one of them is a key the
//! integration tests already connect with (`tests/s3.rs`, `tests/backends.rs`).

use std::collections::BTreeMap;

use crate::profile::{Profile, ProfileId};
use crate::{Error, Result};

/// How a field is entered, which is also how it is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// Masked on screen. Stored like any other option — see [`crate::profile`]
    /// for where that lands and what it means.
    Secret,
    /// A path on this machine. Still just a string to OpenDAL, but the form can
    /// say so.
    Path,
    /// A two-state option whose values are strings, not booleans:
    /// `enable_virtual_host_style` wants `"true"`/`"false"` and
    /// `known_hosts_strategy` wants `"accept"`/`"strict"`.
    Toggle {
        on: &'static str,
        off: &'static str,
    },
}

/// Where a field's value ends up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// An entry in `Profile::options`.
    Option,
    /// The bucket or container — the authority part of the URI.
    UriHost,
    /// The prefix or remote path — everything after the host.
    UriPrefix,
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// Option key for [`Role::Option`]; for the URI roles it only names the
    /// input, since the value is composed into the URI instead.
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub role: Role,
    pub required: bool,
    /// Shown under the input. Single line — a `\n` in a placeholder aborts the
    /// process (see `docs/DESIGN.md`), and keeping hints to one line too means
    /// the two can never be confused.
    pub hint: &'static str,
    /// For a [`Role::UriPrefix`] field: the value is an absolute path, so the
    /// form shows it with its leading `/`. An object-store prefix is relative
    /// and does not want one.
    pub absolute: bool,
}

impl Field {
    const fn text(key: &'static str, label: &'static str, hint: &'static str) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Text,
            role: Role::Option,
            required: false,
            hint,
            absolute: false,
        }
    }

    const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    const fn secret(mut self) -> Self {
        self.kind = FieldKind::Secret;
        self
    }

    const fn path(mut self) -> Self {
        self.kind = FieldKind::Path;
        self
    }

    const fn toggle(mut self, on: &'static str, off: &'static str) -> Self {
        self.kind = FieldKind::Toggle { on, off };
        self
    }

    const fn host(mut self) -> Self {
        self.role = Role::UriHost;
        self
    }

    const fn prefix(mut self) -> Self {
        self.role = Role::UriPrefix;
        self
    }

    const fn absolute(mut self) -> Self {
        self.absolute = true;
        self
    }

    pub fn is_secret(&self) -> bool {
        matches!(self.kind, FieldKind::Secret)
    }

    /// The value a fresh form starts with, so a toggle is not silently empty.
    pub fn default_value(&self) -> &'static str {
        match self.kind {
            FieldKind::Toggle { off, .. } => off,
            _ => "",
        }
    }
}

pub struct Service {
    pub scheme: &'static str,
    pub label: &'static str,
    pub fields: &'static [Field],
}

impl Service {
    pub fn host_field(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.role == Role::UriHost)
    }

    pub fn prefix_field(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.role == Role::UriPrefix)
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }
}

/// Every backend the app is built with, in the order the picker shows them.
pub static SERVICES: &[Service] = &[
    Service {
        scheme: "fs",
        label: "本机磁盘",
        // OpenDAL's fs service takes the directory from the URI itself, so this
        // composes `fs:///path` rather than riding along as a `root` option —
        // which also keeps a hand-written `fs:///tmp` profile valid.
        fields: &[Field::text("root", "目录", "例如 /Users/you/Documents")
            .path()
            .prefix()
            .absolute()
            .required()],
    },
    Service {
        scheme: "s3",
        label: "S3 / 兼容对象存储",
        fields: &[
            Field::text("bucket", "Bucket", "存储桶名称")
                .host()
                .required(),
            Field::text("prefix", "前缀", "可选，例如 team/reports").prefix(),
            Field::text("endpoint", "Endpoint", "自建或兼容服务填写，AWS 可留空"),
            Field::text("region", "区域", "例如 us-east-1"),
            Field::text("access_key_id", "Access Key ID", "").required(),
            Field::text("secret_access_key", "Secret Access Key", "")
                .secret()
                .required(),
            Field::text(
                "enable_virtual_host_style",
                "使用 virtual-host 寻址",
                "MinIO 等兼容服务通常需要关闭",
            )
            .toggle("true", "false"),
        ],
    },
    Service {
        scheme: "gcs",
        label: "Google Cloud Storage",
        fields: &[
            Field::text("bucket", "Bucket", "存储桶名称")
                .host()
                .required(),
            Field::text("prefix", "前缀", "可选").prefix(),
            Field::text("endpoint", "Endpoint", "使用模拟器时填写，正式环境留空"),
            Field::text("token", "访问令牌", "OAuth2 token")
                .secret()
                .required(),
        ],
    },
    Service {
        scheme: "azblob",
        label: "Azure Blob Storage",
        fields: &[
            Field::text("container", "容器", "container 名称")
                .host()
                .required(),
            Field::text("prefix", "前缀", "可选").prefix(),
            Field::text("account_name", "账户名", "storage account 名称").required(),
            Field::text("account_key", "账户密钥", "")
                .secret()
                .required(),
            Field::text("endpoint", "Endpoint", "使用 Azurite 时填写，正式环境留空"),
        ],
    },
    Service {
        scheme: "webdav",
        label: "WebDAV",
        fields: &[
            Field::text("endpoint", "服务地址", "例如 https://dav.example.com").required(),
            Field::text("prefix", "路径", "可选，服务地址之后的子路径").prefix(),
            Field::text("username", "用户名", ""),
            Field::text("password", "密码", "").secret(),
        ],
    },
    Service {
        scheme: "sftp",
        label: "SFTP",
        fields: &[
            Field::text("endpoint", "主机", "例如 example.com:22").required(),
            Field::text("path", "远端路径", "例如 /upload")
                .prefix()
                .absolute(),
            Field::text("user", "用户名", "").required(),
            Field::text("key", "私钥文件", "本机路径，SFTP 需要密钥在磁盘上")
                .path()
                .required(),
            Field::text(
                "known_hosts_strategy",
                "接受未知主机密钥",
                "关闭则使用系统 known_hosts",
            )
            .toggle("accept", "strict"),
        ],
    },
];

pub fn for_scheme(scheme: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.scheme == scheme)
}

/// Assemble a profile from what the form collected.
///
/// The URI is composed rather than typed: `scheme://{host}/{prefix}`, which
/// covers every service — a bucket-shaped one fills the host, `webdav` and
/// `sftp` leave it empty and keep only a path, and `fs` has neither, landing on
/// the `fs:///` the local session already uses.
pub fn build_profile(
    id: ProfileId,
    name: String,
    scheme: &str,
    values: &BTreeMap<String, String>,
) -> Result<Profile> {
    let service =
        for_scheme(scheme).ok_or_else(|| Error::Config(format!("未知的服务类型 {scheme}")))?;

    let mut options = BTreeMap::new();
    let mut host = String::new();
    let mut prefix = String::new();

    for field in service.fields {
        let value = values.get(field.key).map(|v| v.trim()).unwrap_or("");

        if field.required && value.is_empty() {
            return Err(Error::Config(format!("请填写{}", field.label)));
        }

        match field.role {
            Role::UriHost => host = value.to_string(),
            Role::UriPrefix => prefix = value.trim_matches('/').to_string(),
            // Two things are left out rather than written. An empty optional
            // option, because "" is a value to some services and not a default.
            // And a toggle sitting at its off value, because writing it would
            // mean editing a profile to change its region also stamped an
            // opinion on `enable_virtual_host_style` that the user never gave —
            // a round-trip through the form has to leave a profile unchanged.
            Role::Option if value.is_empty() => {}
            Role::Option if matches!(field.kind, FieldKind::Toggle { off, .. } if value == off) => {
            }
            Role::Option => {
                options.insert(field.key.to_string(), value.to_string());
            }
        }
    }

    let profile = Profile {
        id,
        name,
        uri: format!("{scheme}://{host}/{prefix}"),
        options,
    };
    profile.validate()?;
    Ok(profile)
}

/// Split a profile back into form values, for editing.
///
/// Secrets come back too — they are in the profile, so there is nothing to hide
/// from the person who saved them, and a form that silently blanked them would
/// make "change the region" delete the password.
pub fn field_values(profile: &Profile) -> BTreeMap<String, String> {
    let mut values: BTreeMap<String, String> = profile
        .options
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let Some(service) = for_scheme(profile.scheme()) else {
        return values;
    };

    let rest = profile
        .uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("");

    if let Some(field) = service.host_field() {
        let (host, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        values.insert(field.key.to_string(), host.to_string());
        if let Some(prefix_field) = service.prefix_field() {
            values.insert(prefix_field.key.to_string(), prefix.to_string());
        }
    } else if let Some(prefix_field) = service.prefix_field() {
        let prefix = rest.trim_start_matches('/');
        values.insert(
            prefix_field.key.to_string(),
            if prefix_field.absolute {
                format!("/{prefix}")
            } else {
                prefix.to_string()
            },
        );
    }

    // A toggle with nothing stored has to read as its off value, or the form
    // would show it on.
    for field in service.fields {
        if let FieldKind::Toggle { off, .. } = field.kind {
            values
                .entry(field.key.to_string())
                .or_insert_with(|| off.to_string());
        }
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn s3_composes_bucket_and_prefix_into_the_uri() {
        let profile = build_profile(
            "p".into(),
            "MinIO".into(),
            "s3",
            &values(&[
                ("bucket", "roam-test"),
                ("prefix", "team/reports"),
                ("access_key_id", "key"),
                ("secret_access_key", "secret"),
            ]),
        )
        .unwrap();

        assert_eq!(profile.uri, "s3://roam-test/team/reports");
        assert_eq!(profile.options.get("access_key_id").unwrap(), "key");
        // Not written as an empty string — absent.
        assert!(!profile.options.contains_key("region"));
    }

    #[test]
    fn a_service_without_a_host_keeps_only_a_path() {
        let profile = build_profile(
            "p".into(),
            "DAV".into(),
            "webdav",
            &values(&[
                ("endpoint", "https://dav.example.com"),
                ("prefix", "/files/"),
            ]),
        )
        .unwrap();

        assert_eq!(profile.uri, "webdav:///files");
    }

    #[test]
    fn fs_puts_the_directory_in_the_uri() {
        let profile = build_profile(
            "p".into(),
            "本机".into(),
            "fs",
            &values(&[("root", "/tmp/x")]),
        )
        .unwrap();

        assert_eq!(profile.uri, "fs:///tmp/x");
        assert!(profile.options.is_empty(), "no options needed for fs");

        // And it comes back with its leading slash, not as "tmp/x".
        assert_eq!(field_values(&profile).get("root").unwrap(), "/tmp/x");
    }

    #[test]
    fn a_missing_required_field_names_itself() {
        let err = build_profile("p".into(), "n".into(), "s3", &values(&[("bucket", "b")]))
            .unwrap_err()
            .user_message();

        assert!(err.contains("Access Key ID"), "unhelpful message: {err}");
    }

    #[test]
    fn round_trips_through_field_values() {
        let original = build_profile(
            "p".into(),
            "MinIO".into(),
            "s3",
            &values(&[
                ("bucket", "b"),
                ("prefix", "p/q"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("access_key_id", "key"),
                ("secret_access_key", "secret"),
                ("enable_virtual_host_style", "false"),
            ]),
        )
        .unwrap();

        let back = field_values(&original);
        assert_eq!(back.get("bucket").unwrap(), "b");
        assert_eq!(back.get("prefix").unwrap(), "p/q");
        assert_eq!(back.get("secret_access_key").unwrap(), "secret");

        let again = build_profile(original.id.clone(), original.name.clone(), "s3", &back).unwrap();
        assert_eq!(again, original);
    }

    #[test]
    fn a_toggle_left_alone_reads_as_off() {
        let profile = build_profile(
            "p".into(),
            "本机".into(),
            "fs",
            &values(&[("root", "/tmp")]),
        )
        .unwrap();
        // fs has no toggle; sftp does, and an unsaved one must not read as on.
        let sftp = build_profile(
            "s".into(),
            "SFTP".into(),
            "sftp",
            &values(&[("endpoint", "h:22"), ("user", "u"), ("key", "/tmp/k")]),
        )
        .unwrap();

        assert!(!profile.options.contains_key("known_hosts_strategy"));
        assert_eq!(
            field_values(&sftp).get("known_hosts_strategy").unwrap(),
            "strict"
        );
    }

    #[test]
    fn every_service_has_a_label_and_at_least_one_required_field() {
        for service in SERVICES {
            assert!(!service.label.is_empty(), "{} has no label", service.scheme);
            assert!(
                service.fields.iter().any(|f| f.required),
                "{} asks for nothing",
                service.scheme
            );
        }
    }

    /// The hints and placeholders share a rule with the input placeholders: no
    /// newlines, because gpui-component aborts the process on a multi-line one.
    #[test]
    fn no_field_text_contains_a_newline() {
        for service in SERVICES {
            for field in service.fields {
                assert!(!field.label.contains('\n'), "{} label", field.key);
                assert!(!field.hint.contains('\n'), "{} hint", field.key);
            }
        }
    }
}

#[cfg(test)]
mod round_trip_tests {
    use super::*;

    /// The bug this caught: a toggle left alone was written as its off value, so
    /// opening a profile to change the region also stamped
    /// `enable_virtual_host_style = false` onto it. Editing has to be a no-op
    /// when nothing was edited.
    #[test]
    fn editing_without_changing_anything_leaves_the_profile_identical() {
        let mut original = Profile::new("p", "MinIO", "s3://bucket/data");
        original.options.insert("region".into(), "us-east-1".into());
        original
            .options
            .insert("access_key_id".into(), "AKIA".into());
        original
            .options
            .insert("secret_access_key".into(), "shhh".into());

        let back = field_values(&original);
        let again = build_profile(original.id.clone(), original.name.clone(), "s3", &back).unwrap();

        assert_eq!(again, original);
    }

    #[test]
    fn a_toggle_turned_on_is_written() {
        let mut values = field_values(&Profile::new("p", "n", "s3://b/"));
        values.insert("access_key_id".into(), "k".into());
        values.insert("secret_access_key".into(), "s".into());
        values.insert("enable_virtual_host_style".into(), "true".into());

        let profile = build_profile("p".into(), "n".into(), "s3", &values).unwrap();
        assert_eq!(
            profile.options.get("enable_virtual_host_style").unwrap(),
            "true"
        );
    }
}
