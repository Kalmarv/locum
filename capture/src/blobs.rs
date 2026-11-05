//! The blob store: contents named by their [`crate::hash::key`], shared by every capture that points at it. A blob is
//! written once; captures of the same textures add nothing, so one store serves every run of a game.
//!
//! Blobs are appended to pack files, `<root>/packs/<process>-<time>.pack`, one per writing process, and
//! `<root>/index.db` (SQLite, table `blobs(key, pack, offset, size)`) says where each one is. A capture's hundreds of
//! thousands of blobs are then a few large sequential writes and one transaction, not a file each. Stores written
//! before packs hold one file per blob at `<root>/<first two digits>/<key>`; those are still read.
//!
//! The index also lists the captures that use the store (table `captures(path)`): a capture is listed before its
//! `capture.db` is written. [`Blobs::collect`] keeps the blobs those captures name and drops the rest, so deleting a
//! run's captures and collecting gives their space back. Every process using a store holds `<root>/store.lock`
//! shared; collecting takes it alone, so it never drops a blob a running game has just found there and is about to
//! name.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// A pack is closed and the next one begun past this size.
const PACK_LIMIT: u64 = 4 << 30;

/// The file every process using a store holds locked, shared; [`Blobs::collect`] holds it alone.
const LOCK: &str = "store.lock";

#[derive(Clone, Debug)]
pub struct Blobs {
    root: PathBuf,
}

/// Where a blob is: a pack and its bytes there, or a file of its own (older stores).
enum Place {
    Pack { pack: String, offset: u64, size: u64 },
    File(PathBuf),
}

/// A blob in the store: its key and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub size: u64,
}

/// What [`Blobs::copy_from`] did: blobs copied (and their bytes), and blobs the store had already.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Copied {
    pub copied: u64,
    pub copied_bytes: u64,
    pub present: u64,
}

/// What [`Blobs::collect`] kept and dropped (or would, when only counting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub kept: u64,
    pub kept_bytes: u64,
    pub dropped: u64,
    pub dropped_bytes: u64,
    /// Packs holding dropped blobs, their kept blobs copied to new packs and the old ones deleted.
    pub packs_rewritten: u64,
    /// Pack files no index entry names (what a writer that stopped before indexing left), deleted, and their bytes.
    pub orphan_packs: u64,
    pub orphan_bytes: u64,
    /// One-file blobs of an older store moved into packs.
    pub files_packed: u64,
    /// Index entries whose pack is not there: left as they are.
    pub missing: u64,
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

    /// Flushes, then lets go of the store's files (its pack, index and lock) until it is next used: before its
    /// folder is deleted, or for another process to collect it.
    pub fn close(&self) -> io::Result<()> {
        self.store().close()
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

    /// Every blob in the store, indexed and one-file, sorted by key.
    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        let mut store = self.store();
        store.flush()?;
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        if let Some(index) = store.index(false)? {
            let mut s = index.prepare("SELECT key, size FROM blobs").map_err(sql)?;
            let rows = s
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))
                .map_err(sql)?;
            for row in rows {
                let (key, size) = row.map_err(sql)?;
                out.insert(key, size);
            }
        }
        for (key, path) in files(&self.root)? {
            if let std::collections::btree_map::Entry::Vacant(e) = out.entry(key) {
                e.insert(std::fs::metadata(path)?.len());
            }
        }
        Ok(out.into_iter().map(|(key, size)| Entry { key, size }).collect())
    }

    /// Lists `capture` (a `capture.db`) among the captures using the store, so [`Blobs::collect`] keeps what it names.
    pub fn register(&self, capture: &Path) -> io::Result<()> {
        let path = std::path::absolute(capture)?;
        let mut store = self.store();
        let index = store.index(true)?.expect("a writable index");
        index
            .execute(
                "INSERT OR IGNORE INTO captures VALUES(?1)",
                [path.to_string_lossy()],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// Takes `capture` off the list of captures using the store.
    pub fn unregister(&self, capture: &Path) -> io::Result<()> {
        let mut store = self.store();
        let index = store.index(true)?.expect("a writable index");
        index
            .execute(
                "DELETE FROM captures WHERE path = ?1",
                [capture.to_string_lossy()],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// The captures listed as using the store, as they were listed (absolute paths).
    pub fn captures(&self) -> io::Result<Vec<PathBuf>> {
        let mut store = self.store();
        let Some(index) = store.index(false)? else {
            return Ok(Vec::new());
        };
        let listed: bool = index
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'captures'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sql)?
            > 0;
        if !listed {
            return Ok(Vec::new());
        }
        let mut s = index
            .prepare("SELECT path FROM captures ORDER BY path")
            .map_err(sql)?;
        let rows = s.query_map([], |r| r.get::<_, String>(0)).map_err(sql)?;
        rows.map(|r| r.map(PathBuf::from).map_err(sql)).collect()
    }

    /// Copies every blob of `from` this store lacks, then checks each of `from`'s blobs is here with its size.
    pub fn copy_from(&self, from: &Blobs) -> io::Result<Copied> {
        let entries = from.entries()?;
        let mut done = Copied::default();
        for e in &entries {
            if let Some(size) = self.size(&e.key) {
                if size != e.size {
                    return Err(io::Error::other(format!(
                        "blob {} is {} bytes in {} and {size} in {}",
                        e.key,
                        e.size,
                        from.root.display(),
                        self.root.display()
                    )));
                }
                done.present += 1;
                continue;
            }
            let bytes = from.get(&e.key)?;
            self.append(&e.key, &bytes)?;
            done.copied += 1;
            done.copied_bytes += bytes.len() as u64;
            if self.unflushed() > 1 << 30 {
                self.flush()?;
            }
        }
        self.flush()?;
        for e in &entries {
            if self.size(&e.key) != Some(e.size) {
                return Err(io::Error::other(format!(
                    "blob {} did not arrive in {}",
                    e.key,
                    self.root.display()
                )));
            }
        }
        Ok(done)
    }

    /// Keeps the blobs `keep` says yes to and drops the rest: packs holding dropped blobs have their kept blobs copied
    /// to new packs and are deleted, one-file blobs are packed (or deleted), and packs no entry names are deleted.
    /// With `count_only`, says what it would do and changes nothing. Refused while another process uses the store.
    pub fn collect(&self, keep: &dyn Fn(&str) -> bool, count_only: bool) -> io::Result<Collected> {
        let mut store = self.store();
        store.close()?;
        if !self.root.is_dir() {
            return Ok(Collected::default());
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(LOCK))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::other(format!(
                    "{} is in use by another process (a game or locum-cli): collect it when none is",
                    self.root.display()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        // `store` stays locked: nothing else in this process uses the store until this returns.
        collect(&self.root, keep, count_only)
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

/// The one-file blobs of an older store: `<root>/<first two digits>/<key>`.
fn files(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for dir in dirs {
        let dir = dir?;
        let name = dir.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) || !dir.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(dir.path())? {
            let file = file?;
            let key = file.file_name().to_string_lossy().into_owned();
            if key.starts_with(&name) && file.file_type()?.is_file() {
                out.push((key, file.path()));
            }
        }
    }
    Ok(out)
}

/// A new pack of this process's under `<root>/packs`: its name and file.
fn new_pack(root: &Path) -> io::Result<(String, File)> {
    let packs = root.join("packs");
    std::fs::create_dir_all(&packs)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    for n in 0.. {
        let name = match n {
            0 => format!("{}-{millis}.pack", std::process::id()),
            n => format!("{}-{millis}-{n}.pack", std::process::id()),
        };
        match File::options()
            .create_new(true)
            .append(true)
            .open(packs.join(&name))
        {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// The packs [`collect`] writes kept blobs to, a new one past [`PACK_LIMIT`].
struct Out {
    root: PathBuf,
    pack: Option<(String, BufWriter<File>, u64)>,
}

impl Out {
    fn write(&mut self, bytes: &[u8]) -> io::Result<(String, u64)> {
        if self
            .pack
            .as_ref()
            .is_some_and(|(_, _, len)| *len > 0 && *len + bytes.len() as u64 > PACK_LIMIT)
        {
            self.sync()?;
            self.pack = None;
        }
        if self.pack.is_none() {
            let (name, file) = new_pack(&self.root)?;
            self.pack = Some((name, BufWriter::with_capacity(16 << 20, file), 0));
        }
        let (name, file, len) = self.pack.as_mut().expect("a pack");
        file.write_all(bytes)?;
        let at = *len;
        *len += bytes.len() as u64;
        Ok((name.clone(), at))
    }

    /// On disk before the index names what was written.
    fn sync(&mut self) -> io::Result<()> {
        if let Some((_, file, _)) = self.pack.as_mut() {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        Ok(())
    }
}

fn collect(root: &Path, keep: &dyn Fn(&str) -> bool, count_only: bool) -> io::Result<Collected> {
    let mut done = Collected::default();
    let index_path = root.join("index.db");
    let loose = files(root)?;
    if !index_path.is_file() && (loose.is_empty() || count_only) {
        for (key, path) in &loose {
            let size = std::fs::metadata(path)?.len();
            if keep(key) {
                done.kept += 1;
                done.kept_bytes += size;
                done.files_packed += 1;
            } else {
                done.dropped += 1;
                done.dropped_bytes += size;
            }
        }
        return Ok(done);
    }
    let index = Connection::open(&index_path).map_err(sql)?;
    index
        .busy_timeout(std::time::Duration::from_secs(30))
        .map_err(sql)?;
    index.pragma_update(None, "journal_mode", "WAL").map_err(sql)?;
    index
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs(key TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, \
             size INTEGER NOT NULL) WITHOUT ROWID",
        )
        .map_err(sql)?;
    let mut packs: BTreeMap<String, Vec<(String, u64, u64)>> = BTreeMap::new();
    {
        let mut s = index
            .prepare("SELECT key, pack, offset, size FROM blobs")
            .map_err(sql)?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })
            .map_err(sql)?;
        for row in rows {
            let (key, pack, offset, size) = row.map_err(sql)?;
            packs.entry(pack).or_default().push((key, offset, size));
        }
    }
    let dir = root.join("packs");
    // Packs on disk no entry names.
    if let Ok(found) = std::fs::read_dir(&dir) {
        for f in found {
            let f = f?;
            let name = f.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pack") && !packs.contains_key(&name) {
                done.orphan_packs += 1;
                done.orphan_bytes += f.metadata()?.len();
                if !count_only {
                    std::fs::remove_file(f.path())?;
                }
            }
        }
    }
    let mut out = Out {
        root: root.to_path_buf(),
        pack: None,
    };
    for (pack, mut blobs) in packs {
        let path = dir.join(&pack);
        if !path.is_file() {
            done.missing += blobs.len() as u64;
            continue;
        }
        let (kept, dropped): (Vec<_>, Vec<_>) = blobs.drain(..).partition(|(key, _, _)| keep(key));
        done.kept += kept.len() as u64;
        done.kept_bytes += kept.iter().map(|b| b.2).sum::<u64>();
        done.dropped += dropped.len() as u64;
        done.dropped_bytes += dropped.iter().map(|b| b.2).sum::<u64>();
        if dropped.is_empty() {
            continue;
        }
        done.packs_rewritten += 1;
        if count_only {
            continue;
        }
        let mut moved = Vec::with_capacity(kept.len());
        let mut file = File::open(&path)?;
        let mut sorted = kept;
        sorted.sort_by_key(|b| b.1);
        for (key, offset, size) in sorted {
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; size as usize];
            file.read_exact(&mut bytes)?;
            let (to, at) = out.write(&bytes)?;
            moved.push((key, to, at));
        }
        drop(file);
        out.sync()?;
        let tx = index.unchecked_transaction().map_err(sql)?;
        {
            let mut update = tx
                .prepare_cached("UPDATE blobs SET pack = ?2, offset = ?3 WHERE key = ?1")
                .map_err(sql)?;
            for (key, to, at) in &moved {
                update.execute(params![key, to, *at as i64]).map_err(sql)?;
            }
            let mut delete = tx
                .prepare_cached("DELETE FROM blobs WHERE key = ?1")
                .map_err(sql)?;
            for (key, _, _) in &dropped {
                delete.execute([key]).map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        std::fs::remove_file(&path)?;
    }
    let mut packed = Vec::new();
    let mut removed = Vec::new();
    for (key, path) in loose {
        let size = std::fs::metadata(&path)?.len();
        let indexed = index
            .query_row("SELECT 1 FROM blobs WHERE key = ?1", [&key], |_| Ok(()))
            .optional()
            .map_err(sql)?
            .is_some();
        if indexed {
            // A pack has it already: the file is a second copy.
            done.orphan_bytes += size;
            removed.push(path);
        } else if keep(&key) {
            done.kept += 1;
            done.kept_bytes += size;
            done.files_packed += 1;
            if !count_only {
                let (to, at) = out.write(&std::fs::read(&path)?)?;
                packed.push((key, to, at, size, path));
            }
        } else {
            done.dropped += 1;
            done.dropped_bytes += size;
            removed.push(path);
        }
    }
    if !count_only {
        out.sync()?;
        let tx = index.unchecked_transaction().map_err(sql)?;
        {
            let mut insert = tx
                .prepare_cached("INSERT INTO blobs VALUES(?1, ?2, ?3, ?4)")
                .map_err(sql)?;
            for (key, to, at, size, _) in &packed {
                insert
                    .execute(params![key, to, *at as i64, *size as i64])
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        for path in packed.iter().map(|p| &p.4).chain(&removed) {
            std::fs::remove_file(path)?;
            if let Some(parent) = path.parent() {
                // Gone once its last blob is.
                let _ = std::fs::remove_dir(parent);
            }
        }
        index
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(sql)?;
    }
    Ok(done)
}

struct Store {
    root: PathBuf,
    /// `<root>/store.lock`, held shared while this process uses the store.
    lock: Option<File>,
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
            lock: None,
            index: None,
            pack: None,
            pending: HashMap::new(),
            unflushed: 0,
            readers: HashMap::new(),
        }
    }

    /// Takes the store's lock, shared, if it is not held: waits while a collection runs. A store not there yet, or
    /// one this process cannot write the lock of (read-only media), is used without.
    fn hold(&mut self) -> io::Result<()> {
        if self.lock.is_some() || !self.root.is_dir() {
            return Ok(());
        }
        let Ok(lock) = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(LOCK))
        else {
            return Ok(());
        };
        lock.lock_shared()?;
        self.lock = Some(lock);
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        self.flush()?;
        self.pack = None;
        self.readers.clear();
        self.index = None;
        self.lock = None;
        Ok(())
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
            self.hold()?;
            let c =
                Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(sql)?;
            c.busy_timeout(std::time::Duration::from_secs(30)).map_err(sql)?;
            if write {
                c.pragma_update(None, "journal_mode", "WAL").map_err(sql)?;
                c.pragma_update(None, "synchronous", "NORMAL").map_err(sql)?;
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS blobs(key TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, \
                     size INTEGER NOT NULL) WITHOUT ROWID;
                     CREATE TABLE IF NOT EXISTS captures(path TEXT PRIMARY KEY) WITHOUT ROWID;",
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
        self.hold().ok()?;
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
            std::fs::create_dir_all(&self.root)?;
            self.hold()?;
            let (name, file) = new_pack(&self.root)?;
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
            self.hold()?;
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

    /// A fresh folder for a test: process ids are reused, so one an earlier run left behind is removed first.
    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("locum-blobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn put_get() {
        let dir = fresh("test");
        let blobs = Blobs::new(&dir);
        let key = crate::hash::key(b"abc");
        assert!(blobs.put(&key, b"abc").unwrap());
        assert!(!blobs.put(&key, b"abc").unwrap());
        assert_eq!(blobs.get(&key).unwrap(), b"abc");
        assert_eq!(blobs.read(&key, 1, Some(1)).unwrap(), b"b");
        assert_eq!(blobs.size(&key), Some(3));
        blobs.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Appended blobs are readable at once in this process, and in another (a fresh store on the same folder) once
    /// flushed; one blob after another lands in one pack.
    #[test]
    fn append_flush_and_older_stores() {
        let dir = fresh("pack");
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
        blobs.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copying one store into another brings what it lacks; the captures listed stay listed by their absolute path.
    #[test]
    fn copy_and_register() {
        let (a, b) = (fresh("copy-a"), fresh("copy-b"));
        let (from, into) = (Blobs::new(&a), Blobs::new(&b));
        let shared = crate::hash::key(b"shared");
        let only = crate::hash::key(b"only in a");
        from.put(&shared, b"shared").unwrap();
        from.put(&only, b"only in a").unwrap();
        into.put(&shared, b"shared").unwrap();
        let done = into.copy_from(&from).unwrap();
        assert_eq!(
            done,
            Copied {
                copied: 1,
                copied_bytes: 9,
                present: 1
            }
        );
        assert_eq!(into.get(&only).unwrap(), b"only in a");
        let capture = b.join("run").join("capture.db");
        into.register(&capture).unwrap();
        into.register(&capture).unwrap();
        assert_eq!(
            into.captures().unwrap(),
            vec![std::path::absolute(&capture).unwrap()]
        );
        into.unregister(&into.captures().unwrap()[0]).unwrap();
        assert!(into.captures().unwrap().is_empty());
        from.close().unwrap();
        into.close().unwrap();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Collecting keeps what is asked for, wherever it was (packs, one-file blobs), drops the rest and deletes the
    /// packs that held it; what it kept reads back the same from a fresh process's view of the store.
    #[test]
    fn collect_keeps_what_is_named() {
        let dir = fresh("collect");
        let blobs = Blobs::new(&dir);
        let data: Vec<Vec<u8>> = (0..50u32).map(|i| format!("blob {i}").into_bytes()).collect();
        let keys: Vec<String> = data.iter().map(|d| crate::hash::key(d)).collect();
        for (k, d) in keys.iter().zip(&data) {
            blobs.append(k, d).unwrap();
        }
        blobs.flush().unwrap();
        let loose_kept = crate::hash::key(b"old kept");
        let loose_dropped = crate::hash::key(b"old dropped");
        for (k, d) in [
            (&loose_kept, &b"old kept"[..]),
            (&loose_dropped, &b"old dropped"[..]),
        ] {
            std::fs::create_dir_all(dir.join(&k[..2])).unwrap();
            std::fs::write(dir.join(&k[..2]).join(k), d).unwrap();
        }
        std::fs::write(
            dir.join("packs").join("1-1.pack"),
            b"left by a writer that stopped",
        )
        .unwrap();
        let kept: std::collections::HashSet<String> = keys
            .iter()
            .step_by(2)
            .cloned()
            .chain([loose_kept.clone()])
            .collect();
        let keep = |k: &str| kept.contains(k);
        let counted = blobs.collect(&keep, true).unwrap();
        assert_eq!((counted.kept, counted.dropped, counted.orphan_packs), (26, 26, 1));
        assert!(blobs.has(&keys[1]), "counting changes nothing");
        let done = blobs.collect(&keep, false).unwrap();
        assert_eq!(done, counted);
        assert_eq!(done.files_packed, 1);
        let mut other = Store::new(dir.clone());
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(other.find(k).is_some(), i % 2 == 0, "blob {i}");
        }
        drop(other);
        for (i, (k, d)) in keys.iter().zip(&data).enumerate().step_by(2) {
            assert_eq!(&blobs.get(k).unwrap(), d, "blob {i}");
        }
        assert_eq!(blobs.get(&loose_kept).unwrap(), b"old kept");
        assert!(!blobs.has(&loose_dropped));
        assert!(!dir.join(&loose_kept[..2]).join(&loose_kept).exists());
        // One pack of what was kept; the old pack and the orphan are gone.
        assert_eq!(std::fs::read_dir(dir.join("packs")).unwrap().count(), 1);
        // Nothing left to drop.
        let again = blobs.collect(&keep, false).unwrap();
        assert_eq!((again.dropped, again.packs_rewritten, again.kept), (0, 0, 26));
        blobs.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store another process holds is not collected.
    #[test]
    fn collect_refused_while_held() {
        let dir = fresh("held");
        let blobs = Blobs::new(&dir);
        blobs.put(&crate::hash::key(b"x"), b"x").unwrap();
        blobs.close().unwrap();
        // Another process's hold, as a second handle on the lock.
        let other = File::options()
            .read(true)
            .write(true)
            .open(dir.join(LOCK))
            .unwrap();
        other.lock_shared().unwrap();
        let e = blobs.collect(&|_| false, false).unwrap_err();
        assert!(e.to_string().contains("in use"), "{e}");
        drop(other);
        assert_eq!(blobs.collect(&|_| false, false).unwrap().dropped, 1);
        blobs.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
