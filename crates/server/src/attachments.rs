//! The node's file store for attachments (spec 2.7 §2a, phase K6): the nota photos of cash
//! movements. Files live **on disk, not in the database**, under the node's data directory (the
//! directory of the `SQLite` file, or of `sultan.toml` for `MySQL`):
//! `attachments/<first 2 hex of sha256>/<sha256>.jpg`.
//!
//! - **Content-addressed:** the name is the SHA-256 of the bytes, so the same content is stored
//!   once, whoever uploads it and under whatever photo ID.
//! - **Atomic:** a file is written to a temporary file in the same directory, flushed to disk and
//!   renamed into place; a reader never sees half a file. The route writes the file **before** the
//!   database row, so a failed write leaves no row, and a crash between the two leaves an orphan
//!   file only, which the next upload of the same content reuses ([`Attachments::store`] checks
//!   an existing file's hash and rewrites it when it does not match).
//! - **Owner-only:** files are `0600` and directories `0700` on Unix (a nota may show names and
//!   amounts).
//!
//! The HQ push of the bytes (`PUT /api/v1/sync/files/{sha256}`) comes with the sync worker; the
//! outbox already carries `cash_movement.photo_added` with the hash. Backup / restore (2.12) must
//! include this directory.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The directory under the data directory.
pub const DIR: &str = "attachments";

/// The file store of a node (see the module docs). Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachments {
    root: PathBuf,
}

/// The SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// `hash` in lower-case hex.
#[must_use]
pub fn hex(hash: &[u8; 32]) -> String {
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl Attachments {
    /// The store under `data_dir` (`<data_dir>/attachments`); nothing is created until a file is
    /// stored.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(DIR),
        }
    }

    /// The `attachments` directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the file of `hash` lives.
    #[must_use]
    pub fn path(&self, hash: &[u8; 32]) -> PathBuf {
        let name = hex(hash);
        self.root.join(&name[..2]).join(format!("{name}.jpg"))
    }

    /// Stores `data` (see the module docs) and returns its hash. An existing file with the same
    /// content is kept as it is.
    ///
    /// # Errors
    /// The IO error (the directory cannot be made, the disk is full, …); nothing is left behind
    /// but possibly an empty directory.
    pub fn store(&self, data: &[u8]) -> io::Result<[u8; 32]> {
        let hash = sha256(data);
        let path = self.path(&hash);
        if fs::read(&path).is_ok_and(|stored| sha256(&stored) == hash) {
            return Ok(hash);
        }
        let dir = path.parent().unwrap_or(&self.root);
        create_private_dir(&self.root)?;
        create_private_dir(dir)?;
        let temp = dir.join(format!(
            ".{}.{}.tmp",
            hex(&hash),
            uuid::Uuid::now_v7().simple()
        ));
        let written = write_private(&temp, data).and_then(|()| fs::rename(&temp, &path));
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written.map(|()| hash)
    }

    /// The bytes of the file of `hash`.
    ///
    /// # Errors
    /// The IO error; [`io::ErrorKind::NotFound`] when the file is not on this node.
    pub fn read(&self, hash: &[u8; 32]) -> io::Result<Vec<u8>> {
        fs::read(self.path(hash))
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent)?;
            }
            create_private_dir(dir)
        }
        Err(e) => Err(e),
    }
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file: File = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sultan-attach-{name}-{}",
                uuid::Uuid::now_v7().simple()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_are_content_addressed_private_and_stored_once() {
        let dir = Dir::new("store");
        let store = Attachments::new(&dir.0);
        assert_eq!(store.root(), dir.0.join("attachments"));
        let hash = store.store(b"nota").unwrap();
        assert_eq!(hash, sha256(b"nota"));
        let path = store.path(&hash);
        let name = hex(&hash);
        assert_eq!(
            path,
            dir.0
                .join("attachments")
                .join(&name[..2])
                .join(format!("{name}.jpg"))
        );
        assert_eq!(store.read(&hash).unwrap(), b"nota");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(store.root()), 0o700);
        }
        // The same content again: one file, no temporary left.
        assert_eq!(store.store(b"nota").unwrap(), hash);
        let files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(
            store.read(&sha256(b"other")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_damaged_file_is_rewritten_and_an_orphan_reused() {
        let dir = Dir::new("orphan");
        let store = Attachments::new(&dir.0);
        let hash = sha256(b"photo");
        let path = store.path(&hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // An orphan of the right content (a crash after the write) is kept.
        fs::write(&path, b"photo").unwrap();
        assert_eq!(store.store(b"photo").unwrap(), hash);
        // A damaged one is replaced.
        fs::write(&path, b"phot").unwrap();
        assert_eq!(store.store(b"photo").unwrap(), hash);
        assert_eq!(store.read(&hash).unwrap(), b"photo");
    }

    #[test]
    fn a_failed_write_is_an_error() {
        let dir = Dir::new("fail");
        // The data directory is a file: nothing can be created under it.
        let file = dir.0.join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let store = Attachments::new(&file);
        assert!(store.store(b"photo").is_err());
        // A nested data directory that does not exist yet is created.
        let nested = Attachments::new(&dir.0.join("a").join("b"));
        assert!(nested.store(b"photo").is_ok());
    }
}
