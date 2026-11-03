//! The blob store: contents named by their [`crate::hash::key`], one file each at `<root>/<first two digits>/<key>`,
//! shared by every capture that points at it. A blob is written once; captures of the same textures add nothing.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct Blobs {
    root: PathBuf,
}

impl Blobs {
    pub fn new(root: impl Into<PathBuf>) -> Blobs {
        Blobs { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2.min(key.len())]).join(key)
    }

    pub fn has(&self, key: &str) -> bool {
        self.path(key).is_file()
    }

    /// Writes `bytes` under `key` unless it is there already. Returns whether it wrote. Safe to call from several
    /// threads and processes at once: each writes a file of its own and renames it into place.
    pub fn put(&self, key: &str, bytes: &[u8]) -> io::Result<bool> {
        let path = self.path(key);
        if path.is_file() {
            return Ok(false);
        }
        let dir = path.parent().expect("a blob path has a folder");
        std::fs::create_dir_all(dir)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = dir.join(format!(
            ".{key}.{}.{}.part",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temp, bytes)?;
        match std::fs::rename(&temp, &path) {
            Ok(()) => Ok(true),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                if path.is_file() { Ok(false) } else { Err(e) }
            }
        }
    }

    pub fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get() {
        let dir = std::env::temp_dir().join(format!("locum-blobs-test-{}", std::process::id()));
        let blobs = Blobs::new(&dir);
        let key = crate::hash::key(b"abc");
        assert!(blobs.put(&key, b"abc").unwrap());
        assert!(!blobs.put(&key, b"abc").unwrap());
        assert_eq!(blobs.get(&key).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
