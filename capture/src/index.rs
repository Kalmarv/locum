//! An index of files by the keys of their bytes, to name what a capture's resources were made from. A resource's
//! upload key, each subresource's key and its contents' key are those of the canonical layout, which for a texture
//! is the order a DDS file stores its bytes in; so a DDS file is keyed whole and mip by mip without converting it,
//! and a texture the game made from fewer mips than the file holds (or from one face) still finds the file through
//! its subresources. Buffers (vertices, indices) are keyed as their bytes.
//!
//! The index knows nothing of games: an extractor adds what it reads under names of its choosing (`locum-cli index`
//! adds loose DDS files; a game's own reader adds what is inside its archives).

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::dds;
use crate::format;
use crate::hash::key;
use crate::layout;

const SCHEMA: &str = "
-- What was added: a texture (from a DDS file) or a buffer, by a name its extractor chose.
CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL,
  format INTEGER, width INTEGER, height INTEGER, depth INTEGER, mips INTEGER, array INTEGER,
  bytes INTEGER NOT NULL, key TEXT NOT NULL);
-- Each subresource of a texture, in the canonical layout.
CREATE TABLE IF NOT EXISTS subs(key TEXT NOT NULL, file INTEGER NOT NULL, sub INTEGER NOT NULL, mip INTEGER,
  slice INTEGER, width INTEGER, height INTEGER, bytes INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS files_key ON files(key);
CREATE INDEX IF NOT EXISTS subs_key ON subs(key);
";

/// A file in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub format: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub array: u32,
}

/// A subresource of a file in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSub {
    pub file: File,
    pub sub: u32,
    pub mip: u32,
    pub slice: u32,
}

pub struct Index {
    db: Connection,
}

/// What a resource is, so it is matched only with files that could have made it: a buffer with buffers; a texture
/// with textures of its size whose texels are laid out alike (the same blocks of the same bytes: `BC7_UNORM` and
/// `BC7_UNORM_SRGB`, or any two formats of four bytes a texel). The same bytes in another arrangement are not the
/// same data, only the same bytes (all zeros, say).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Like {
    Buffer,
    Texture {
        formats: Vec<u32>,
        width: u32,
        height: u32,
    },
}

impl Like {
    /// A texture of `format`, `width` x `height` texels (a subresource's size, for [`Index::sub`]).
    pub fn texture(format: u32, width: u32, height: u32) -> Like {
        let layout = format::layout(format);
        let formats = match layout {
            Some(_) => (0..=255).filter(|&f| format::layout(f) == layout).collect(),
            None => Vec::new(),
        };
        Like::Texture {
            formats,
            width,
            height,
        }
    }

    /// SQL that keeps the rows of `table` (files or subs) this matches.
    fn filter(&self, table: &str) -> String {
        match self {
            Like::Buffer => "f.kind = 'buffer'".into(),
            Like::Texture {
                formats,
                width,
                height,
            } => {
                let formats: Vec<String> = formats.iter().map(u32::to_string).collect();
                format!(
                    "f.kind = 'texture' AND f.format IN ({}) AND {table}.width = {width} AND {table}.height = {height}",
                    formats.join(",")
                )
            }
        }
    }
}

const FILE_COLUMNS: &str = "f.id, f.name, f.kind, f.format, f.width, f.height, f.mips, f.array";

fn file_from(r: &rusqlite::Row) -> rusqlite::Result<File> {
    Ok(File {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        format: r.get(3)?,
        width: r.get::<_, Option<u32>>(4)?.unwrap_or(0),
        height: r.get::<_, Option<u32>>(5)?.unwrap_or(0),
        mips: r.get::<_, Option<u32>>(6)?.unwrap_or(1),
        array: r.get::<_, Option<u32>>(7)?.unwrap_or(1),
    })
}

impl Index {
    /// Opens the index at `path` to add to it, making it if there is none.
    pub fn create(path: &Path) -> Result<Index, String> {
        let db = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;")
            .and_then(|()| db.execute_batch(SCHEMA))
            .and_then(|()| db.execute_batch("BEGIN"))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Index { db })
    }

    /// Opens the index at `path` to read it.
    pub fn open(path: &Path) -> Result<Index, String> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Index { db })
    }

    /// Writes what was added since the index was opened (or since the last call).
    pub fn commit(&mut self) -> Result<(), String> {
        self.db.execute_batch("COMMIT; BEGIN").map_err(|e| e.to_string())
    }

    /// Adds a DDS file under `name`, keyed whole and by subresource.
    pub fn add_dds(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let (t, payload) = dds::read(bytes)?;
        let subs = layout::texture(t.format, t.width, t.height, t.depth, t.mips, t.array)
            .ok_or_else(|| format!("format {} has no canonical layout", t.format))?;
        let total = layout::total(&subs) as usize;
        let payload = payload.get(..total).ok_or_else(|| {
            format!(
                "{} bytes after the header, {total} needed for {}x{} format {} with {} mips and {} slices",
                payload.len(),
                t.width,
                t.height,
                t.format,
                t.mips,
                t.array
            )
        })?;
        let e = |e: rusqlite::Error| e.to_string();
        self.db
            .prepare_cached(
                "INSERT INTO files(name, kind, format, width, height, depth, mips, array, bytes, key) \
                 VALUES (?1, 'texture', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .and_then(|mut s| {
                s.execute(params![
                    name,
                    t.format,
                    t.width,
                    t.height,
                    t.depth,
                    t.mips,
                    t.array,
                    total as i64,
                    key(payload)
                ])
            })
            .map_err(e)?;
        let file = self.db.last_insert_rowid();
        let mut insert = self
            .db
            .prepare_cached(
                "INSERT INTO subs(key, file, sub, mip, slice, width, height, bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(e)?;
        for s in &subs {
            let bytes = &payload[s.offset as usize..(s.offset + s.size) as usize];
            insert
                .execute(params![
                    key(bytes),
                    file,
                    s.index,
                    s.mip,
                    s.slice,
                    s.width,
                    s.height,
                    s.size as i64
                ])
                .map_err(e)?;
        }
        Ok(())
    }

    /// Adds a buffer's bytes under `name`.
    pub fn add_buffer(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.add_buffer_key(name, bytes.len() as u64, &key(bytes))
    }

    /// Adds a buffer of `bytes` bytes whose key (`hash::key`) another tool computed, under `name`.
    pub fn add_buffer_key(&mut self, name: &str, bytes: u64, key: &str) -> Result<(), String> {
        if key.len() != 32
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!("{key:?} is not a key: 32 lowercase hex digits"));
        }
        self.db
            .prepare_cached("INSERT INTO files(name, kind, bytes, key) VALUES (?1, 'buffer', ?2, ?3)")
            .and_then(|mut s| s.execute(params![name, bytes as i64, key]))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The files like `like` whose whole bytes have `key`, at most `limit`, and how many there are.
    pub fn whole(&self, key: &str, like: &Like, limit: usize) -> (Vec<File>, usize) {
        let filter = like.filter("f");
        let count = self
            .db
            .query_row(
                &format!("SELECT count(*) FROM files f WHERE f.key = ?1 AND {filter}"),
                [key],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize;
        if count == 0 {
            return (Vec::new(), 0);
        }
        let files = self
            .db
            .prepare_cached(&format!(
                "SELECT {FILE_COLUMNS} FROM files f WHERE f.key = ?1 AND {filter} ORDER BY f.name LIMIT ?2"
            ))
            .and_then(|mut s| {
                s.query_map(params![key, limit as i64], file_from)?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        (files, count)
    }

    /// A subresource like `like` (a texture subresource's format and size) of a file, whose bytes have `key`, and how
    /// many there are.
    pub fn sub(&self, key: &str, like: &Like) -> Option<(FileSub, usize)> {
        let filter = like.filter("s");
        let count = self
            .db
            .query_row(
                &format!(
                    "SELECT count(*) FROM subs s JOIN files f ON f.id = s.file WHERE s.key = ?1 AND {filter}"
                ),
                [key],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize;
        if count == 0 {
            return None;
        }
        self.db
            .prepare_cached(&format!(
                "SELECT {FILE_COLUMNS}, s.sub, s.mip, s.slice FROM subs s JOIN files f ON f.id = s.file \
                 WHERE s.key = ?1 AND {filter} ORDER BY f.name, s.sub LIMIT 1"
            ))
            .and_then(|mut s| {
                s.query_row([key], |r| {
                    Ok(FileSub {
                        file: file_from(r)?,
                        sub: r.get(8)?,
                        mip: r.get(9)?,
                        slice: r.get(10)?,
                    })
                })
                .optional()
            })
            .ok()
            .flatten()
            .map(|s| (s, count))
    }

    /// How many files of each kind the index holds.
    pub fn counts(&self) -> Vec<(String, usize)> {
        self.db
            .prepare("SELECT kind, count(*) FROM files GROUP BY kind ORDER BY kind")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        let _ = self.db.execute_batch("COMMIT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_texture_is_found_whole_and_by_mip() {
        let path = std::env::temp_dir().join(format!("locum-index-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let t = dds::Texture {
            format: 87,
            dimensions: 2,
            width: 4,
            height: 2,
            depth: 1,
            mips: 2,
            array: 1,
            cube: false,
        };
        // 4x2 and 2x1 texels of four bytes.
        let payload: Vec<u8> = (0..40u8).collect();
        {
            let mut index = Index::create(&path).unwrap();
            index.add_dds("a/b.dds", &dds::file(&t, &payload)).unwrap();
            index.add_buffer("a/vertices", b"vertex bytes").unwrap();
            assert!(
                index
                    .add_dds("short.dds", &dds::file(&t, &payload[..20]))
                    .is_err()
            );
        }
        let index = Index::open(&path).unwrap();
        // R8G8B8A8_UNORM (28) is laid out as B8G8R8A8_UNORM (87) is: four bytes a texel.
        let (files, count) = index.whole(&key(&payload), &Like::texture(28, 4, 2), 10);
        assert_eq!((files[0].name.as_str(), count), ("a/b.dds", 1));
        let (s, _) = index
            .sub(&key(&payload[32..40]), &Like::texture(87, 2, 1))
            .unwrap();
        assert_eq!((s.file.name.as_str(), s.mip, s.slice), ("a/b.dds", 1, 0));
        assert_eq!(
            index.whole(&key(b"vertex bytes"), &Like::Buffer, 10).0[0].kind,
            "buffer"
        );
        assert!(index.sub(&key(b"nothing"), &Like::texture(87, 2, 1)).is_none());
        // The same bytes are not the same data as a buffer, in another layout, or at another size.
        assert_eq!(index.whole(&key(&payload), &Like::Buffer, 10).1, 0);
        assert_eq!(
            index.whole(&key(b"vertex bytes"), &Like::texture(87, 4, 2), 10).1,
            0
        );
        assert!(
            index
                .sub(&key(&payload[32..40]), &Like::texture(56, 2, 2))
                .is_none()
        );
        assert!(
            index
                .sub(&key(&payload[32..40]), &Like::texture(87, 1, 2))
                .is_none()
        );
        assert!(index.sub(&key(&payload[32..40]), &Like::Buffer).is_none());
        assert_eq!(
            index.counts(),
            vec![("buffer".to_string(), 1), ("texture".to_string(), 1)]
        );
        drop(index);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_listed_key_names_a_buffer() {
        let path = std::env::temp_dir().join(format!("locum-index-keys-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let bytes = b"a mesh's vertices";
        {
            let mut index = Index::create(&path).unwrap();
            index
                .add_buffer_key("m.flver#vb0", bytes.len() as u64, &key(bytes))
                .unwrap();
            assert!(index.add_buffer_key("bad", 1, "not a key").is_err());
            assert!(index.add_buffer_key("upper", 1, &key(bytes).to_uppercase()).is_err());
        }
        let index = Index::open(&path).unwrap();
        let (files, count) = index.whole(&key(bytes), &Like::Buffer, 10);
        assert_eq!((files[0].name.as_str(), count), ("m.flver#vb0", 1));
        drop(index);
        let _ = std::fs::remove_file(&path);
    }
}
