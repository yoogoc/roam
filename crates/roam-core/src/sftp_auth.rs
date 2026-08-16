//! Password authentication for the sftp backend.
//!
//! # Why this exists
//!
//! OpenDAL's sftp service has no password option. Its config is `endpoint`,
//! `root`, `user`, `key`, `known_hosts_strategy`, `enable_copy` — and underneath
//! it builds an `openssh::SessionBuilder`, whose authentication settings are
//! `keyfile`, `user` and `ssh_auth_sock`. That is because it does not speak the
//! protocol itself: it runs the system `ssh`, and `ssh` accepts no password on
//! its command line, by design.
//!
//! What `ssh` *does* accept is a helper program: with `SSH_ASKPASS` set and
//! `SSH_ASKPASS_REQUIRE=force`, it asks that program for the password instead of
//! reading a terminal. OpenSSH 8.4 and later need no `DISPLAY` for this
//! (verified against 10.3, which is what macOS ships). Since the `ssh` OpenDAL
//! spawns is our child process, it inherits our environment.
//!
//! That is not sufficient on its own. `openssh` builds its command line with
//! `-o BatchMode=yes` hardcoded, and `BatchMode` is precisely the option that
//! disables password prompting — including the askpass path. Measured against a
//! real server: with `BatchMode=yes` the same connection that otherwise succeeds
//! answers `Permission denied (publickey,password,keyboard-interactive)`. It
//! cannot be undone from outside either, because `ssh` keeps the *first* value it
//! is given for an option, and that one is already on the command line before
//! anything we could add.
//!
//! So the second half of this: `openssh` finds `ssh` on `PATH`, which means we can
//! put our own in front of it. The shim drops that one option and `exec`s the real
//! `ssh` with everything else untouched — argv is rotated rather than
//! re-quoted, so arguments containing spaces (the control socket lives in a temp
//! directory) survive exactly. It is installed only when a profile actually has a
//! password, so key-based connections keep `BatchMode` and its fast failure.
//!
//! # How a password reaches the right connection
//!
//! `SSH_ASKPASS` is process-global, but the app can have several sftp sessions
//! open at once, each with its own password. `ssh` passes its prompt to the
//! helper as a single argument:
//!
//! ```text
//! pwuser@127.0.0.1's password:
//! ```
//!
//! so the helper can work out *which* connection is asking and look the password
//! up per host. It reads it from an environment variable named after the
//! connection, which keeps passwords out of any file — including out of the
//! helper script itself.
//!
//! # What this costs
//!
//! The password sits in this process's environment for as long as the app runs.
//! On both macOS and Linux another user cannot read it; the same user can. That
//! is the same exposure as `profiles.toml`, which already holds it on disk (see
//! [`crate::profile`]), so this adds no new class of risk — but it is worth
//! knowing rather than assuming.

use std::io::Write;
use std::path::PathBuf;

use crate::{Error, Result};

/// The helper. Contains no secret: it derives an environment variable name from
/// the prompt `ssh` hands it and prints whatever that variable holds.
///
/// Kept byte-identical in spirit to [`env_key`] — the test at the bottom runs
/// this very script and checks the two agree, because a mismatch would look like
/// a wrong password rather than a bug.
const ASKPASS: &str = r#"#!/bin/sh
# Written by roam. ssh passes its prompt as the only argument, for example
# "user@host's password: ". The password itself lives in the environment.
target=$(printf '%s' "$1" | sed -e "s/'s password:.*$//" -e "s/^.*[[:space:]]//")
key=$(printf '%s' "$target" | tr 'a-z' 'A-Z' | tr -c 'A-Z0-9\n' '_')
eval "printf '%s\n' \"\${ROAM_SFTP_PW_${key}}\""
"#;

/// The variable a password for `user@host` is read from.
///
/// Upper-cased with everything outside `[A-Z0-9]` replaced, so the result is a
/// legal environment variable name. Two hosts differing only in punctuation
/// would collide; hosts that differ that way are not a case worth designing for,
/// and the alternative — hashing — cannot be done in POSIX `sh`.
pub fn env_key(user: &str, host: &str) -> String {
    let target = format!("{user}@{}", host_only(host));
    let mut key = String::with_capacity(target.len());
    for ch in target.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_uppercase());
        } else {
            key.push('_');
        }
    }
    format!("ROAM_SFTP_PW_{key}")
}

/// The hostname `ssh` will name in its prompt: no scheme, no port.
///
/// The prompt showed `pwuser@127.0.0.1` for an endpoint reached on port 12222, so
/// the port has to come off or the lookup misses.
fn host_only(endpoint: &str) -> &str {
    let host = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    let host = host.split('/').next().unwrap_or(host);

    // An IPv6 literal keeps its brackets and colons; only strip a trailing
    // `:port` from something that is not bracketed.
    if host.starts_with('[') {
        return host;
    }
    match host.rsplit_once(':') {
        Some((before, port)) if port.chars().all(|c| c.is_ascii_digit()) => before,
        _ => host,
    }
}

/// The `ssh` shim. Everything except one option passes straight through.
const SSH_SHIM: &str = r#"#!/bin/sh
# Written by roam. `openssh` hardcodes `-o BatchMode=yes`, which disables the
# password prompting we need; ssh keeps the first value it sees for an option, so
# it has to be removed rather than overridden. Nothing else is touched.
#
# argv is rotated one argument at a time instead of being rebuilt as a string,
# because paths here contain spaces.
n=$#
i=0
while [ "$i" -lt "$n" ]; do
    if [ "$1" = "-o" ] && [ "$2" = "BatchMode=yes" ]; then
        shift 2
        n=$((n - 2))
        continue
    fi
    a="$1"
    shift
    set -- "$@" "$a"
    i=$((i + 1))
done
exec "$ROAM_REAL_SSH" "$@"
"#;

/// Private directory holding both helpers, keyed by pid so two running copies of
/// the app cannot tread on each other.
fn helper_dir() -> PathBuf {
    std::env::temp_dir().join(format!("roam-ssh-{}", std::process::id()))
}

fn helper_path() -> PathBuf {
    helper_dir().join("askpass")
}

fn shim_path() -> PathBuf {
    helper_dir().join("ssh")
}

/// Where the real `ssh` lives, before our shim shadows it.
fn real_ssh() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let shim_dir = helper_dir();

    std::env::split_paths(&path)
        .filter(|dir| dir != &shim_dir)
        .map(|dir| dir.join("ssh"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| Error::Config("PATH 上找不到 ssh，SFTP 需要它".into()))
}

#[cfg(unix)]
fn write_executable(path: &PathBuf, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut file = std::fs::File::create(path)
        .map_err(|e| Error::Config(format!("无法写入 ssh 助手: {e}")))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| Error::Config(format!("无法写入 ssh 助手: {e}")))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Config(format!("无法设置助手权限: {e}")))
}

#[cfg(not(unix))]
fn write_executable(path: &PathBuf, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| Error::Config(format!("无法写入 ssh 助手: {e}")))
}

/// Make `password` available to the next `ssh` for `user@endpoint`.
///
/// Writes the helper if it is not there yet and points `SSH_ASKPASS` at it.
pub fn install(user: &str, endpoint: &str, password: &str) -> Result<()> {
    let dir = helper_dir();
    let askpass = helper_path();
    let shim = shim_path();

    if !askpass.exists() || !shim.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Config(format!("无法创建 ssh 助手目录: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        write_executable(&askpass, ASKPASS)?;
        write_executable(&shim, SSH_SHIM)?;
    }

    // Resolved before the shim is on PATH, or it would find itself.
    let real = real_ssh()?;
    let key = env_key(user, endpoint);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let already_shimmed = std::env::split_paths(&path).any(|entry| entry == dir);

    // SAFETY: `set_var` is unsafe because another thread reading the environment
    // at the same moment is a data race. This runs on the caller's thread while
    // building a session, before any `ssh` is spawned, and the only reader is
    // that future child process — which gets a copy at `fork`.
    unsafe {
        std::env::set_var(&key, password);
        std::env::set_var("SSH_ASKPASS", &askpass);
        // Without `force`, ssh only consults the helper when it has no terminal.
        // The app has none, but a `cargo run` from a shell does.
        std::env::set_var("SSH_ASKPASS_REQUIRE", "force");
        std::env::set_var("ROAM_REAL_SSH", &real);

        if !already_shimmed {
            let mut entries = vec![dir];
            entries.extend(std::env::split_paths(&path));
            let joined = std::env::join_paths(entries)
                .map_err(|e| Error::Config(format!("无法设置 PATH: {e}")))?;
            std::env::set_var("PATH", joined);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_not_part_of_the_prompt() {
        // ssh prompts for `user@host` even when a port was given, so the key must
        // not include one — this was measured against a real server on 12222.
        assert_eq!(
            env_key("pwuser", "127.0.0.1:12222"),
            "ROAM_SFTP_PW_PWUSER_127_0_0_1"
        );
        assert_eq!(
            env_key("pwuser", "127.0.0.1"),
            "ROAM_SFTP_PW_PWUSER_127_0_0_1"
        );
    }

    #[test]
    fn a_scheme_and_path_come_off_too() {
        assert_eq!(
            env_key("me", "ssh://example.com:22/upload"),
            "ROAM_SFTP_PW_ME_EXAMPLE_COM"
        );
    }

    #[test]
    fn an_ipv6_literal_keeps_its_shape() {
        // `[::1]` must not lose the address to the port-stripping rule. Four
        // separators between the user and the digit: `@`, `[`, `:`, `:`.
        assert_eq!(env_key("me", "[::1]"), "ROAM_SFTP_PW_ME____1_");
    }

    /// The one test that matters: the shell in [`ASKPASS`] and the Rust in
    /// [`env_key`] have to derive the same name. If they drift, every password
    /// silently becomes the empty string and the failure looks like bad
    /// credentials.
    #[cfg(unix)]
    #[test]
    fn the_helper_script_derives_the_same_key_as_we_do() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("askpass.sh");
        std::fs::write(&script, ASKPASS).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        for (user, host) in [
            ("pwuser", "127.0.0.1"),
            ("me", "example.com"),
            ("dash-user", "my-host.internal"),
        ] {
            let key = env_key(user, host);
            let out = Command::new(&script)
                .arg(format!("{user}@{host}'s password: "))
                .env(&key, "the-secret")
                .output()
                .unwrap();

            let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert_eq!(
                printed, "the-secret",
                "helper looked up a different variable than {key} for {user}@{host}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_helper_prints_nothing_when_no_password_is_set() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("askpass.sh");
        std::fs::write(&script, ASKPASS).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        // A key-authenticated connection must not be handed a stray password.
        let out = Command::new(&script)
            .arg("someone@nowhere's password: ")
            .env_remove(env_key("someone", "nowhere"))
            .output()
            .unwrap();

        assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    }
}
