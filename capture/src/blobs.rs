//! The blob store: contents named by their [`crate::hash::key`], shared by every capture that points at it. A blob is
//! written once; captures of the same textures add nothing.
//!
//! Blobs are appended to pack files, `<root>/packs/<process>-<time>.pack`, one per writing process, and
//! `<root>/index.db` (SQLite, table `blobs(key, pack, offset, size)`) says where each one is. A capture's hundreds of
//! thousands of blobs are then a few large sequential writes and one transaction, not a file each. Stores written
//! before packs hold one file per blob at `<root>/<first two digits>/<key>`; those are still read.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// A pack is closed and the next one begun past this size.
const PACK_LIMIT: u64 = 4 << 30;

#[derive(Clone, Debug)]
pub struct Blobs {
    root: PathBuf,
}

/// Where a blob is: a pack and its bytes there, or a file of its own (older stores).
enum Place {
    Pack { pack: String, offset: u64, size: u64 },
    File(PathBuf),
}

impl Blobs {
    pub fn new(root: impl Into<PathBuf>) -> Blobs {
        Blobs { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This process's state for the store at `root`, made once and kept for the life of the process.
    fn store(&self) -> MutexGuard<'static, Store> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<Store>>>> = OnceLock::new();
        let store: &'static Mutex<Store> = STORES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(self.root.clone())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(Store::new(self.root.clone())))));
        store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn has(&self, key: &str) -> bool {
        self.store().find(key).is_some()
    }

    /// The size of a blob in the store.
    pub fn size(&self, key: &str) -> Option<u64> {
        match self.store().find(key)? {
            Place::Pack { size, .. } => Some(size),
            Place::File(path) => std::fs::metadata(path).ok().map(|m| m.len()),
        }
    }

    /// Writes `bytes` under `key` unless it is there already, and makes it visible to other processes. Returns
    /// whether it wrote.
    pub fn put(&self, key: &str, bytes: &[u8]) -> io::Result<bool> {
        let mut store = self.store();
        let wrote = store.append(key, bytes)?;
        store.flush()?;
        Ok(wrote)
    }

    /// Appends `bytes` under `key` unless it is there already; other processes see it after [`Blobs::flush`].
    pub fn append(&self, key: &str, bytes: &[u8]) -> io::Result<bool> {
        self.store().append(key, bytes)
    }

    /// Makes everything appended so far visible to other processes.
    pub fn flush(&self) -> io::Result<()> {
        self.store().flush()
    }

    /// Bytes appended since the last flush.
    pub fn unflushed(&self) -> u64 {
        self.store().unflushed
    }

    pub fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.read(key, 0, None)
    }

    /// `size` bytes of a blob from `offset` (the rest of it without a size).
    pub fn read(&self, key: &str, offset: u64, size: Option<u64>) -> io::Result<Vec<u8>> {
        let mut store = self.store();
        if store.pending.contains_key(key) {
            store.flush()?;
        }
        let missing = || io::Error::new(io::ErrorKind::NotFound, format!("blob {key} is not in the store"));
        match store.find(key).ok_or_else(missing)? {
            Place::Pack {
                pack,
                offset: at,
                size: total,
            } => {
                let n = size
                    .unwrap_or(total.saturating_sub(offset))
                    .min(total.saturating_sub(offset));
                let file = store.reader(&pack)?;
                file.seek(SeekFrom::Start(at + offset.min(total)))?;
                let mut out = vec![0; n as usize];
                file.read_exact(&mut out)?;
                Ok(out)
            }
            Place::File(path) => {
                let mut file = File::open(path)?;
                let total = file.metadata()?.len();
                let n = size
                    .unwrap_or(total.saturating_sub(offset))
                    .min(total.saturating_sub(offset));
                file.seek(SeekFrom::Start(offset.min(total)))?;
                let mut out = vec![0; n as usize];
                file.read_exact(&mut out)?;
                Ok(out)
            }
        }
    }

    /// Forgets a blob as if it had never been written (tests of what happens when the store loses one).
    #[cfg(test)]
    pub(crate) fn forget(&self, key: &str) {
        let mut store = self.store();
        store.flush().unwrap();
        if let Some(index) = store.index(false).unwrap() {
            index.execute("DELETE FROM blobs WHERE key = ?1", [key]).unwrap();
        }
    }
}

struct Store {
    root: PathBuf,
    /// The index: opened read-only until this process writes, then read-write (and created).
    index: Option<(Connection, bool)>,
    /// This process's pack being appended to: its name, the file, and its length.
    pack: Option<(String, BufWriter<File>, u64)>,
    /// Blobs appended but not yet in the index.
    pending: HashMap<String, (String, u64, u64)>,
    unflushed: u64,
    readers: HashMap<String, File>,
}

impl Store {
    fn new(root: PathBuf) -> Store {
        Store {
            root,
            index: None,
            pack: None,
            pending: HashMap::new(),
            unflushed: 0,
            readers: HashMap::new(),
        }
    }

    /// The index, writable if `write` (created if need be); None when reading a store that has none.
    fn index(&mut self, write: bool) -> io::Result<Option<&Connection>> {
        let path = self.root.join("index.db");
        if self.index.as_ref().is_some_and(|(_, w)| write && !*w) {
            self.index = None;
        }
        if self.index.is_none() {
            if !write && !path.is_file() {
                return Ok(None);
            }
            let flags = if write {
                std::fs::create_dir_all(&self.root)?;
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            };
            let c =
                Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(sql)?;
            c.busy_timeout(std::time::Duration::from_secs(30)).map_err(sql)?;
            if write {
                c.pragma_update(None, "journal_mode", "WAL").map_err(sql)?;
                c.pragma_update(None, "synchronous", "NORMAL").map_err(sql)?;
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS blobs(key TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, \
                     size INTEGER NOT NULL) WITHOUT ROWID",
                )
                .map_err(sql)?;
            }
            self.index = Some((c, write));
        }
        Ok(self.index.as_ref().map(|(c, _)| c))
    }

    fn find(&mut self, key: &str) -> Option<Place> {
        if let Some((pack, offset, size)) = self.pending.get(key) {
            return Some(Place::Pack {
                pack: pack.clone(),
                offset: *offset,
                size: *size,
            });
        }
        if let Ok(Some(index)) = self.index(false) {
            let found = index
                .prepare_cached("SELECT pack, offset, size FROM blobs WHERE key = ?1")
                .and_then(|mut s| {
                    s.query_row([key], |r| {
                        Ok(Place::Pack {
                            pack: r.get(0)?,
                            offset: r.get::<_, i64>(1)? as u64,
                            size: r.get::<_, i64>(2)? as u64,
                        })
                    })
                    .optional()
                })
                .ok()
                .flatten();
            if found.is_some() {
                return found;
            }
        }
        let file = self.root.join(&key[..2.min(key.len())]).join(key);
        file.is_file().then_some(Place::File(file))
    }

    fn append(&mut self, key: &str, bytes: &[u8]) -> io::Result<bool> {
        if self.find(key).is_some() {
            return Ok(false);
        }
        if self
            .pack
            .as_ref()
            .is_some_and(|(_, _, len)| *len + bytes.len() as u64 > PACK_LIMIT)
        {
            self.flush()?;
            self.pack = None;
        }
        if self.pack.is_none() {
            let packs = self.root.join("packs");
            std::fs::create_dir_all(&packs)?;
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let name = format!("{}-{millis}.pack", std::process::id());
            let file = File::options()
                .create_new(true)
                .append(true)
                .open(packs.join(&name))?;
            self.pack = Some((name, BufWriter::with_capacity(16 << 20, file), 0));
        }
        let (name, file, len) = self.pack.as_mut().expect("a pack");
        file.write_all(bytes)?;
        self.pending
            .insert(key.to_owned(), (name.clone(), *len, bytes.len() as u64));
        *len += bytes.len() as u64;
        self.unflushed += bytes.len() as u64;
        Ok(true)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some((_, file, _)) = self.pack.as_mut() {
            file.flush()?;
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let indexed = (|| {
            let index = self.index(true)?.expect("a writable index");
            let tx = index.unchecked_transaction().map_err(sql)?;
            {
                let mut s = tx
                    .prepare_cached("INSERT OR IGNORE INTO blobs VALUES(?1, ?2, ?3, ?4)")
                    .map_err(sql)?;
                for (key, (pack, offset, size)) in &pending {
                    s.execute(params![key, pack, *offset as i64, *size as i64])
                        .map_err(sql)?;
                }
            }
            tx.commit().map_err(sql)
        })();
        match indexed {
            Ok(()) => {
                self.unflushed = 0;
                Ok(())
            }
            Err(e) => {
                // Still in the pack: kept, to be indexed by the next flush.
                self.pending = pending;
                Err(e)
            }
        }
    }

    fn reader(&mut self, pack: &str) -> io::Result<&mut File> {
        if !self.readers.contains_key(pack) {
            let file = File::open(self.root.join("packs").join(pack))?;
            self.readers.insert(pack.to_owned(), file);
        }
        Ok(self.readers.get_mut(pack).expect("just opened"))
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get() {
        let dir = std::env::temp_dir().join(format!("locum-blobs-test-{}", std::process::id()));
        // Process ids are reused: a folder an earlier run left behind must not count as stored blobs.
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = Blobs::new(&dir);
        let key = crate::hash::key(b"abc");
        assert!(blobs.put(&key, b"abc").unwrap());
        assert!(!blobs.put(&key, b"abc").unwrap());
        assert_eq!(blobs.get(&key).unwrap(), b"abc");
        assert_eq!(blobs.read(&key, 1, Some(1)).unwrap(), b"b");
        assert_eq!(blobs.size(&key), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Appended blobs are readable at once in this process, and in another (a fresh store on the same folder) once
    /// flushed; one blob after another lands in one pack.
    #[test]
    fn append_flush_and_older_stores() {
        let dir = std::env::temp_dir().join(format!("locum-blobs-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = Blobs::new(&dir);
        let keys: Vec<String> = (0..100u32).map(|i| crate::hash::key(&i.to_le_bytes())).collect();
        for (i, k) in keys.iter().enumerate() {
            assert!(blobs.append(k, &(i as u32).to_le_bytes()).unwrap());
        }
        assert_eq!(blobs.get(&keys[7]).unwrap(), 7u32.to_le_bytes());
        blobs.flush().unwrap();
        let mut other = Store::new(dir.clone());
        for (i, k) in keys.iter().enumerate() {
            let Some(Place::Pack { offset, size, .. }) = other.find(k) else {
                panic!("{k} not indexed")
            };
            assert_eq!((offset, size), (4 * i as u64, 4));
        }
        assert_eq!(std::fs::read_dir(dir.join("packs")).unwrap().count(), 1);
        // A store written before packs: one file per blob.
        let old = crate::hash::key(b"old");
        std::fs::create_dir_all(dir.join(&old[..2])).unwrap();
        std::fs::write(dir.join(&old[..2]).join(&old), b"old").unwrap();
        assert!(blobs.has(&old));
        assert_eq!(blobs.get(&old).unwrap(), b"old");
        assert!(!blobs.append(&old, b"old").unwrap());
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
