//! The register's crash copy of the cart (spec 2.6 D2): a small JSON file next to
//! `register.token`, written on every change of the cart and restored on the next login for the
//! same cashier session ([`crate::sell::Saved`]).
//!
//! Written like the device token: owner-only (`0600` on Unix), and atomically (a temporary file
//! in the same directory, then a rename), so a crash in the middle never leaves half a cart. A
//! missing or unreadable file is no cart. The file is deleted after a posted sale and after a
//! cart void.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The file name, next to `register.token`.
pub const CART_FILE: &str = "register.cart";

/// The crash-copy file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CartFile {
    path: PathBuf,
}

impl CartFile {
    /// The file at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn temp(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        self.path.with_file_name(name)
    }

    /// Writes `contents` atomically with owner-only permissions.
    ///
    /// # Errors
    /// The IO error.
    pub fn save(&self, contents: &str) -> io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let temp = self.temp();
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            if temp.is_file() {
                fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
            }
        }
        let mut file = options.open(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &self.path)
    }

    /// The contents, or `None` when there is no (readable) file.
    #[must_use]
    pub fn load(&self) -> Option<String> {
        fs::read_to_string(&self.path).ok()
    }

    /// Deletes the file (a missing file is fine).
    ///
    /// # Errors
    /// Any other IO error.
    pub fn delete(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sultan-crash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn save_load_delete() {
        let d = dir("cycle");
        let file = CartFile::new(d.join("sub").join(CART_FILE));
        assert_eq!(file.load(), None);
        file.save("{\"a\":1}").unwrap();
        assert_eq!(file.load().as_deref(), Some("{\"a\":1}"));
        file.save("{}").unwrap();
        assert_eq!(file.load().as_deref(), Some("{}"));
        assert!(!file.temp().exists(), "the temporary file is renamed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(file.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        file.delete().unwrap();
        file.delete().unwrap();
        assert_eq!(file.load(), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn errors_are_reported() {
        let d = dir("err");
        fs::create_dir_all(d.join(CART_FILE)).unwrap();
        let file = CartFile::new(d.join(CART_FILE));
        assert!(file.save("x").is_err(), "a directory is in the way");
        assert!(file.delete().is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
