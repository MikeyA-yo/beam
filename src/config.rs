use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Random bytes in the session token. 20 bytes = 160 bits, above the PRD's 128-bit floor.
const TOKEN_BYTES: usize = 20;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not find your home folder; pass --dir to choose a shared folder")]
    NoHome,
    #[error("could not create the shared folder {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the shared folder {0} exists but is not a folder")]
    NotADir(PathBuf),
    #[error("could not resolve the shared folder {path}: {source}")]
    Resolve {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The shared folder, in two forms.
#[derive(Debug, Clone)]
pub struct SharedDir {
    /// Absolute path for showing to the user.
    pub display: PathBuf,
    /// Canonical path (symlinks resolved) used for containment checks.
    /// On Windows this is a `\\?\` path, which is why it is not shown to the user.
    pub canonical: PathBuf,
}

/// `~/Downloads/Beam`.
pub fn default_dir() -> Result<PathBuf, ConfigError> {
    #[cfg(windows)]
    const HOME_VAR: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME_VAR: &str = "HOME";

    let home = std::env::var_os(HOME_VAR)
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::NoHome)?;
    Ok(PathBuf::from(home).join("Downloads").join("Beam"))
}

/// Creates the folder if missing and resolves both path forms.
pub fn prepare_dir(dir: &Path) -> Result<SharedDir, ConfigError> {
    std::fs::create_dir_all(dir).map_err(|source| ConfigError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;
    if !dir.is_dir() {
        return Err(ConfigError::NotADir(dir.to_path_buf()));
    }
    let resolve = |source| ConfigError::Resolve {
        path: dir.to_path_buf(),
        source,
    };
    let display = std::path::absolute(dir).map_err(resolve)?;
    let canonical = dir.canonicalize().map_err(resolve)?;
    Ok(SharedDir { display, canonical })
}

/// A fresh URL-safe (lowercase hex) session token.
pub fn generate_token() -> String {
    let bytes: [u8; TOKEN_BYTES] = rand::random();
    bytes
        .iter()
        .fold(String::with_capacity(TOKEN_BYTES * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_long_hex_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn prepare_dir_creates_missing_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a").join("b");
        let shared = prepare_dir(&target).unwrap();
        assert!(target.is_dir());
        assert!(shared.display.is_absolute());
        assert_eq!(shared.canonical, target.canonicalize().unwrap());
    }

    #[test]
    fn prepare_dir_rejects_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(prepare_dir(&file).is_err());
    }
}
