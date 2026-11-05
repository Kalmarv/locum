//! The blob store: contents named by their [`crate::hash::key`], shared by every capture that points at it. A blob is
//! written once; captures of the same textures add nothing, so one store serves every run of a game.
//!
//! Blobs are appended to pack files, `<root>/packs/<process>-<time>.pack`, one per writing process, and
//! `<root>/index.db` (SQLite, table `packed(key, pack, offset, stored, size, codec)`) says where each one is: `stored`
//! bytes at `offset` in the pack, which decode by `codec` to the blob's `size` bytes. A capture's hundreds of
//! thousands of blobs are then a few large sequential writes and one transaction, not a file each. Blobs are LZ4
//! blocks (codec `lz4`) unless that saves too little to be worth decoding (codec `raw`): a frame's targets and
//! contents shrink about threefold, block-compressed textures hardly at all.
//!
//! Older stores are still read: their table `blobs(key, pack, offset, size)` holds raw blobs, and stores from before
//! packs hold one file per blob at `<root>/<first two digits>/<key>`. A reader from before `packed` finds none of its
//! blobs, rather than reading compressed bytes as the blob. [`Blobs::collect`] rewrites both older forms into
//! compressed packs.
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

use primase_foundations::compress::{Compressor, Decompressor, Lz4};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// A pack is closed and the next one begun past this size.
const PACK_LIMIT: u64 = 4 << 30;

/// The file every process using a store holds locked, shared; [`Blobs::collect`] holds it alone.
const LOCK: &str = "store.lock";

/// Blobs smaller than this are stored raw: there is too little in them to find repeats in.
const COMPRESS_FROM: usize = 256;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS packed(key TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT \
                      NULL, stored INTEGER NOT NULL, size INTEGER NOT NULL, codec TEXT NOT NULL) WITHOUT ROWID;
                      CREATE TABLE IF NOT EXISTS captures(path TEXT PRIMARY KEY) WITHOUT ROWID;";

#[derive(Clone, Debug)]
pub struct Blobs {
    root: PathBuf,
}

/// How a blob's bytes in its pack decode to the blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Raw,
    Lz4,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Lz4 => "lz4",
        }
    }

    fn parse(name: &str) -> rusqlite::Result<Codec> {
        match name {
            "raw" => Ok(Codec::Raw),
            "lz4" => Ok(Codec::Lz4),
            other => Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("codec {other:?}, which this locum does not read"),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

/// A blob's bytes as stored: compressed when that saves at least a sixteenth, else as they are.
fn encode(bytes: &[u8]) -> io::Result<(std::borrow::Cow<'_, [u8]>, Codec)> {
    if bytes.len() >= COMPRESS_FROM {
        let packed = Lz4.compress(bytes).map_err(io::Error::other)?;
        if packed.len() <= bytes.len() - bytes.len() / 16 {
            return Ok((packed.into(), Codec::Lz4));
        }
    }
    Ok((bytes.into(), Codec::Raw))
}

/// Where a blob is: `stored` bytes in a pack decoding to `size`, or a file of its own (stores from before packs).
#[derive(Clone, Debug)]
enum Place {
    Pack {
        pack: String,
        offset: u64,
        stored: u64,
        size: u64,
        codec: Codec,
    },
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
    /// Blobs kept, and their bytes (decoded).
    pub kept: u64,
    pub kept_bytes: u64,
    pub dropped: u64,
    pub dropped_bytes: u64,
    /// Packs rewritten (holding dropped or uncompressed blobs): their kept blobs copied to new packs, the old deleted.
    pub packs_rewritten: u64,
    /// Blobs of an older store's forms (the table `blobs`, one file each) moved into compressed packs.
    pub converted: u64,
    /// Pack files no index entry names (what a writer that stopped before indexing left), deleted, and their bytes.
    pub orphan_packs: u64,
    pub orphan_bytes: u64,
    /// Index entries whose pack is not there: left as they are.
    pub missing: u64,
    /// The store's blob bytes on disk before and after (when only counting, after is before less what is dropped:
    /// what converting saves is not known until it is done).
    pub disk_before: u64,
    pub disk_after: u64,
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

    /// The size of a blob in the store (decoded).
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

    /// Bytes appended since the last flush (as given, before compression).
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
        let place = store.find(key).ok_or_else(missing)?;
        // The span asked for, within the blob's `total` bytes.
        let span = |total: u64| {
            let from = offset.min(total);
            (from, size.unwrap_or(total - from).min(total - from) as usize)
        };
        match place {
            Place::Pack {
                pack,
                offset: at,
                stored,
                size: total,
                codec,
            } => {
                let (from, n) = span(total);
                let file = store.reader(&pack)?;
                match codec {
                    Codec::Raw => read_at(file, at + from, n),
                    Codec::Lz4 => {
                        let packed = read_at(file, at, stored as usize)?;
                        let mut whole = Lz4
                            .decompress(&packed, total as usize)
                            .map_err(|e| io::Error::other(format!("blob {key}: {e}")))?;
                        whole.truncate(from as usize + n);
                        whole.drain(..from as usize);
                        Ok(whole)
                    }
                }
            }
            Place::File(path) => {
                let file = File::open(path)?;
                let (from, n) = span(file.metadata()?.len());
                read_at(&file, from, n)
            }
        }
    }

    /// Every blob in the store, in each of its forms, sorted by key.
    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        let mut store = self.store();
        store.flush()?;
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        let tables = store.tables()?;
        if let Some(index) = store.index(false)? {
            for (table, present) in [("packed", tables.packed), ("blobs", tables.legacy)] {
                if !present {
                    continue;
                }
                let mut s = index
                    .prepare(&format!("SELECT key, size FROM {table}"))
                    .map_err(sql)?;
                let rows = s
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))
                    .map_err(sql)?;
                for row in rows {
                    let (key, size) = row.map_err(sql)?;
                    out.entry(key).or_insert(size);
                }
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
        if !store.tables()?.captures {
            return Ok(Vec::new());
        }
        let Some(index) = store.index(false)? else {
            return Ok(Vec::new());
        };
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

    /// Keeps the blobs `keep` says yes to and drops the rest. Packs holding dropped blobs, or blobs stored raw by an
    /// older locum, have their kept blobs copied to new packs (compressed where that pays) and are deleted; one-file
    /// blobs are packed (or deleted); packs no entry names are deleted. With `count_only`, says what it would do and
    /// changes nothing. Refused while another process uses the store.
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
            index.execute("DELETE FROM packed WHERE key = ?1", [key]).unwrap();
        }
    }
}

fn read_at(mut file: &File, at: u64, n: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut out = vec![0; n];
    file.read_exact(&mut out)?;
    Ok(out)
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

/// Which of the index's tables are there.
#[derive(Clone, Copy, Default)]
struct Tables {
    packed: bool,
    legacy: bool,
    captures: bool,
}

fn tables(index: &Connection) -> rusqlite::Result<Tables> {
    let mut t = Tables::default();
    let mut s = index.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    for name in s.query_map([], |r| r.get::<_, String>(0))? {
        match name?.as_str() {
            "packed" => t.packed = true,
            "blobs" => t.legacy = true,
            "captures" => t.captures = true,
            _ => {}
        }
    }
    Ok(t)
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

/// An index entry as [`collect`] reads it.
struct Row {
    key: String,
    offset: u64,
    stored: u64,
    size: u64,
    codec: Codec,
    legacy: bool,
}

fn collect(root: &Path, keep: &dyn Fn(&str) -> bool, count_only: bool) -> io::Result<Collected> {
    let mut done = Collected::default();
    let index_path = root.join("index.db");
    let loose = files(root)?;
    if !index_path.is_file() && (loose.is_empty() || count_only) {
        for (key, path) in &loose {
            let size = std::fs::metadata(path)?.len();
            done.disk_before += size;
            if keep(key) {
                done.kept += 1;
                done.kept_bytes += size;
                done.converted += 1;
                done.disk_after += size;
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
    index.execute_batch(SCHEMA).map_err(sql)?;
    let present = tables(&index).map_err(sql)?;
    let mut packs: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut indexed = std::collections::HashSet::new();
    for (table, legacy) in [("packed", false), ("blobs", true)] {
        if legacy && !present.legacy {
            continue;
        }
        let columns = if legacy {
            "key, pack, offset, size, size, 'raw'"
        } else {
            "key, pack, offset, stored, size, codec"
        };
        let mut s = index
            .prepare(&format!("SELECT {columns} FROM {table}"))
            .map_err(sql)?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    Row {
                        key: r.get(0)?,
                        offset: r.get::<_, i64>(2)? as u64,
                        stored: r.get::<_, i64>(3)? as u64,
                        size: r.get::<_, i64>(4)? as u64,
                        codec: Codec::parse(&r.get::<_, String>(5)?)?,
                        legacy,
                    },
                ))
            })
            .map_err(sql)?;
        for row in rows {
            let (pack, row) = row.map_err(sql)?;
            // A key in both tables (an older locum wrote it again): the current table's is the one read.
            if indexed.insert(row.key.clone()) {
                packs.entry(pack).or_default().push(row);
            } else if legacy {
                packs.entry(pack).or_default().push(Row {
                    key: String::new(),
                    ..row
                });
            }
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
    done.disk_before += done.orphan_bytes;
    let mut out = Out {
        root: root.to_path_buf(),
        pack: None,
    };
    for (pack, rows) in packs {
        let path = dir.join(&pack);
        if !path.is_file() {
            done.missing += rows.len() as u64;
            continue;
        }
        let on_disk = std::fs::metadata(&path)?.len();
        done.disk_before += on_disk;
        // A legacy row without a key is a second copy of a blob the current table has: dropped with its pack.
        let (kept, dropped): (Vec<Row>, Vec<Row>) =
            rows.into_iter().partition(|r| !r.key.is_empty() && keep(&r.key));
        let dropped_named = dropped.iter().filter(|r| !r.key.is_empty());
        done.kept += kept.len() as u64;
        done.kept_bytes += kept.iter().map(|r| r.size).sum::<u64>();
        done.dropped += dropped_named.clone().count() as u64;
        done.dropped_bytes += dropped_named.map(|r| r.size).sum::<u64>();
        let converting = kept.iter().filter(|r| r.legacy).count() as u64;
        if dropped.is_empty() && converting == 0 {
            done.disk_after += on_disk;
            continue;
        }
        done.packs_rewritten += 1;
        done.converted += converting;
        if count_only {
            done.disk_after += kept.iter().map(|r| r.stored).sum::<u64>();
            continue;
        }
        let mut moved = Vec::with_capacity(kept.len());
        let file = File::open(&path)?;
        let mut sorted = kept;
        sorted.sort_by_key(|r| r.offset);
        for r in sorted {
            let bytes = read_at(&file, r.offset, r.stored as usize)?;
            let (stored, codec) = if r.legacy {
                encode(&bytes)?
            } else {
                (bytes.as_slice().into(), r.codec)
            };
            let (to, at) = out.write(&stored)?;
            done.disk_after += stored.len() as u64;
            moved.push((r.key, to, at, stored.len() as u64, r.size, codec, r.legacy));
        }
        drop(file);
        out.sync()?;
        let tx = index.unchecked_transaction().map_err(sql)?;
        {
            let mut put = tx
                .prepare_cached("INSERT OR REPLACE INTO packed VALUES(?1, ?2, ?3, ?4, ?5, ?6)")
                .map_err(sql)?;
            let mut unlist = tx
                .prepare_cached(if present.legacy {
                    "DELETE FROM blobs WHERE key = ?1"
                } else {
                    "SELECT ?1 WHERE 0"
                })
                .map_err(sql)?;
            for (key, to, at, stored, size, codec, legacy) in &moved {
                put.execute(params![
                    key,
                    to,
                    *at as i64,
                    *stored as i64,
                    *size as i64,
                    codec.name()
                ])
                .map_err(sql)?;
                if *legacy {
                    unlist.execute([key]).map_err(sql)?;
                }
            }
            let mut drop_current = tx
                .prepare_cached("DELETE FROM packed WHERE key = ?1")
                .map_err(sql)?;
            for r in dropped.iter().filter(|r| !r.key.is_empty()) {
                if r.legacy {
                    unlist.execute([&r.key]).map_err(sql)?;
                } else {
                    drop_current.execute([&r.key]).map_err(sql)?;
                }
            }
            if present.legacy {
                // What else the old table says is in this pack (second copies) goes with it.
                tx.execute("DELETE FROM blobs WHERE pack = ?1", [&pack])
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        std::fs::remove_file(&path)?;
    }
    let mut packed = Vec::new();
    let mut removed = Vec::new();
    for (key, path) in loose {
        let size = std::fs::metadata(&path)?.len();
        done.disk_before += size;
        if indexed.contains(&key) {
            // A pack has it already: the file is a second copy.
            done.orphan_bytes += size;
            removed.push(path);
        } else if keep(&key) {
            done.kept += 1;
            done.kept_bytes += size;
            done.converted += 1;
            if count_only {
                done.disk_after += size;
            } else {
                let bytes = std::fs::read(&path)?;
                let (stored, codec) = encode(&bytes)?;
                let (to, at) = out.write(&stored)?;
                done.disk_after += stored.len() as u64;
                packed.push((key, to, at, stored.len() as u64, size, codec, path));
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
                .prepare_cached("INSERT INTO packed VALUES(?1, ?2, ?3, ?4, ?5, ?6)")
                .map_err(sql)?;
            for (key, to, at, stored, size, codec, _) in &packed {
                insert
                    .execute(params![
                        key,
                        to,
                        *at as i64,
                        *stored as i64,
                        *size as i64,
                        codec.name()
                    ])
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        for path in packed.iter().map(|p| &p.6).chain(&removed) {
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
    /// Which tables the index has, read when it is opened.
    tables: Tables,
    /// This process's pack being appended to: its name, the file, and its length.
    pack: Option<(String, BufWriter<File>, u64)>,
    /// Blobs appended but not yet in the index.
    pending: HashMap<String, Place>,
    unflushed: u64,
    readers: HashMap<String, File>,
}

impl Store {
    fn new(root: PathBuf) -> Store {
        Store {
            root,
            lock: None,
            index: None,
            tables: Tables::default(),
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
                c.execute_batch(SCHEMA).map_err(sql)?;
            }
            self.tables = tables(&c).map_err(sql)?;
            self.index = Some((c, write));
        }
        Ok(self.index.as_ref().map(|(c, _)| c))
    }

    /// The index's tables (none when there is no index).
    fn tables(&mut self) -> io::Result<Tables> {
        Ok(match self.index(false)? {
            Some(_) => self.tables,
            None => Tables::default(),
        })
    }

    fn find(&mut self, key: &str) -> Option<Place> {
        if let Some(place) = self.pending.get(key) {
            return Some(place.clone());
        }
        let tables = self.tables().ok()?;
        if let Ok(Some(index)) = self.index(false) {
            let mut found = None;
            if tables.packed {
                found = index
                    .prepare_cached("SELECT pack, offset, stored, size, codec FROM packed WHERE key = ?1")
                    .and_then(|mut s| {
                        s.query_row([key], |r| {
                            Ok(Place::Pack {
                                pack: r.get(0)?,
                                offset: r.get::<_, i64>(1)? as u64,
                                stored: r.get::<_, i64>(2)? as u64,
                                size: r.get::<_, i64>(3)? as u64,
                                codec: Codec::parse(&r.get::<_, String>(4)?)?,
                            })
                        })
                        .optional()
                    })
                    .ok()
                    .flatten();
            }
            if found.is_none() && tables.legacy {
                found = index
                    .prepare_cached("SELECT pack, offset, size FROM blobs WHERE key = ?1")
                    .and_then(|mut s| {
                        s.query_row([key], |r| {
                            let size = r.get::<_, i64>(2)? as u64;
                            Ok(Place::Pack {
                                pack: r.get(0)?,
                                offset: r.get::<_, i64>(1)? as u64,
                                stored: size,
                                size,
                                codec: Codec::Raw,
                            })
                        })
                        .optional()
                    })
                    .ok()
                    .flatten();
            }
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
        let (stored, codec) = encode(bytes)?;
        if self
            .pack
            .as_ref()
            .is_some_and(|(_, _, len)| *len + stored.len() as u64 > PACK_LIMIT)
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
        file.write_all(&stored)?;
        self.pending.insert(
            key.to_owned(),
            Place::Pack {
                pack: name.clone(),
                offset: *len,
                stored: stored.len() as u64,
                size: bytes.len() as u64,
                codec,
            },
        );
        *len += stored.len() as u64;
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
                    .prepare_cached("INSERT OR IGNORE INTO packed VALUES(?1, ?2, ?3, ?4, ?5, ?6)")
                    .map_err(sql)?;
                for (key, place) in &pending {
                    if let Place::Pack {
                        pack,
                        offset,
                        stored,
                        size,
                        codec,
                        ..
                    } = place
                    {
                        s.execute(params![
                            key,
                            pack,
                            *offset as i64,
                            *stored as i64,
                            *size as i64,
                            codec.name()
                        ])
                        .map_err(sql)?;
                    }
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

    /// Bytes that compress (a pattern) and bytes that do not (an xorshift stream), each `n` long.
    fn compressible(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i / 64) as u8).collect()
    }

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
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

    /// A blob that compresses is stored as an LZ4 block and one that does not as it is; both read back whole and in
    /// part, here and from a fresh process's view of the store.
    #[test]
    fn compressed_and_raw() {
        let dir = fresh("codec");
        let blobs = Blobs::new(&dir);
        let (a, b) = (compressible(100_000), noise(100_000, 7));
        let (ka, kb) = (crate::hash::key(&a), crate::hash::key(&b));
        blobs.put(&ka, &a).unwrap();
        blobs.put(&kb, &b).unwrap();
        let mut other = Store::new(dir.clone());
        let Some(Place::Pack {
            stored, size, codec, ..
        }) = other.find(&ka)
        else {
            panic!("not indexed")
        };
        assert_eq!((codec, size), (Codec::Lz4, 100_000));
        assert!(stored < 10_000, "{stored}");
        let Some(Place::Pack { stored, codec, .. }) = other.find(&kb) else {
            panic!("not indexed")
        };
        assert_eq!((codec, stored), (Codec::Raw, 100_000));
        drop(other);
        assert_eq!(blobs.get(&ka).unwrap(), a);
        assert_eq!(blobs.get(&kb).unwrap(), b);
        assert_eq!(blobs.read(&ka, 640, Some(70)).unwrap(), &a[640..710]);
        assert_eq!(blobs.read(&kb, 99_990, None).unwrap(), &b[99_990..]);
        assert_eq!(blobs.size(&ka), Some(100_000));
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

    /// A store an older locum wrote (raw blobs in the table `blobs`) is read as it is, and collecting moves what it
    /// keeps into compressed packs and the current table, reading back the same.
    #[test]
    fn older_table_read_and_converted() {
        let dir = fresh("legacy");
        std::fs::create_dir_all(dir.join("packs")).unwrap();
        let data: Vec<Vec<u8>> = (0..4).map(|i| compressible(10_000 + i)).collect();
        let keys: Vec<String> = data.iter().map(|d| crate::hash::key(d)).collect();
        let mut pack = Vec::new();
        let index = Connection::open(dir.join("index.db")).unwrap();
        index
            .execute_batch(
                "CREATE TABLE blobs(key TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, size INTEGER \
                 NOT NULL) WITHOUT ROWID",
            )
            .unwrap();
        for (k, d) in keys.iter().zip(&data) {
            index
                .execute(
                    "INSERT INTO blobs VALUES(?1, 'old.pack', ?2, ?3)",
                    params![k, pack.len() as i64, d.len() as i64],
                )
                .unwrap();
            pack.extend_from_slice(d);
        }
        drop(index);
        std::fs::write(dir.join("packs").join("old.pack"), &pack).unwrap();
        let blobs = Blobs::new(&dir);
        assert_eq!(blobs.get(&keys[2]).unwrap(), data[2]);
        assert_eq!(blobs.entries().unwrap().len(), 4);
        let keep = |k: &str| k != keys[3];
        let done = blobs.collect(&keep, false).unwrap();
        assert_eq!(
            (done.kept, done.dropped, done.converted, done.packs_rewritten),
            (3, 1, 3, 1)
        );
        assert!(done.disk_after * 10 < done.disk_before, "{done:?}");
        assert!(!dir.join("packs").join("old.pack").exists());
        let index = Connection::open(dir.join("index.db")).unwrap();
        let legacy: i64 = index
            .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        let current: i64 = index
            .query_row("SELECT count(*) FROM packed WHERE codec = 'lz4'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!((legacy, current), (0, 3));
        drop(index);
        for (k, d) in keys.iter().zip(&data).take(3) {
            assert_eq!(&blobs.get(k).unwrap(), d);
        }
        assert!(!blobs.has(&keys[3]));
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
        let data: Vec<Vec<u8>> = (0..50u32)
            .map(|i| match i % 3 {
                0 => compressible(1000 + i as usize),
                1 => noise(1000, i as u64),
                _ => format!("blob {i}").into_bytes(),
            })
            .collect();
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
        assert_eq!(
            (
                done.kept,
                done.dropped,
                done.orphan_packs,
                done.converted,
                done.disk_before
            ),
            (
                counted.kept,
                counted.dropped,
                counted.orphan_packs,
                counted.converted,
                counted.disk_before
            )
        );
        assert_eq!(done.converted, 1);
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
        // Nothing left to drop or convert.
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
