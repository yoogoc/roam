//! Directories on the machine the app is running on.
//!
//! Deliberately not `std::env::var("HOME")`. Windows has no such variable — the
//! home directory there is a known folder, reachable through `USERPROFILE` or
//! `SHGetKnownFolderPath`, and reading `HOME` simply fails. That failure used to
//! stop the app at launch before a window ever opened.
//!
//! `directories` already knows all of this per platform, and this crate already
//! depends on it for the config directory, so both answers come from there.

use std::path::PathBuf;

use directories::UserDirs;

/// The user's home directory.
pub fn home() -> Option<PathBuf> {
    Some(UserDirs::new()?.home_dir().to_path_buf())
}

/// Where downloads land.
///
/// The known folder when the platform has one — it is relocatable on Windows and
/// configurable on Linux, so `home/Downloads` is a guess, not the answer. But it
/// is a good guess, and on Linux `directories` returns nothing at all unless
/// `~/.config/user-dirs.dirs` exists, so the convention stays as the fallback
/// rather than turning "no XDG config" into "no downloads".
pub fn downloads() -> Option<PathBuf> {
    let user_dirs = UserDirs::new()?;

    if let Some(dir) = user_dirs.download_dir()
        && dir.is_dir()
    {
        return Some(dir.to_path_buf());
    }

    let fallback = user_dirs.home_dir().join("Downloads");
    fallback.is_dir().then_some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app opens on the home directory, so failing to find one stops it at
    /// launch with no window — which is exactly what `std::env::var("HOME")` did
    /// on Windows.
    #[test]
    fn the_home_directory_is_found_on_this_platform() {
        let home = home().expect("no home directory");
        assert!(home.is_dir(), "{} is not a directory", home.display());
    }
}
