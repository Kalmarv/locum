//! Names for a capture's resources from indexes of files (`locum_capture::index`): `index` keys loose DDS files,
//! `join` lists what each resource was made from, and every command that shows a bound resource shows its name when
//! an index is given (`--index`, or `LOCUM_INDEX` with paths separated by `;`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use locum_capture::hash::key;
use locum_capture::index::{File, Index, Like};

use crate::describe::shape;
use crate::open::Capture;
use crate::out;

/// The indexes `--index` names, or else `LOCUM_INDEX`.
pub fn paths(given: &[PathBuf]) -> Vec<PathBuf> {
    if !given.is_empty() {
        return given.to_vec();
    }
    std::env::var("LOCUM_INDEX")
        .map(|v| {
            v.split(';')
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// What a resource was made from.
#[derive(Clone, Debug)]
pub struct Match {
    pub file: File,
    /// How the resource and the file correspond: "upload", "contents", or which subresources.
    pub how: String,
    /// Other files in the indexes with the same bytes.
    pub others: usize,
}

impl std::fmt::Display for Match {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.file.name, self.how)?;
        if self.others > 0 {
            write!(f, " +{} with the same bytes", self.others)?;
        }
        Ok(())
    }
}

/// A vertex binding: its resource, offset and size.
pub type Vb = (i64, Option<i64>, Option<i64>);

/// Whether the frame binds any of bytes `offset..offset + len` of buffer `id` as vertices.
fn holds_vertices(c: &Capture, id: i64, offset: u64, len: u64) -> bool {
    let end = i64::try_from(offset.saturating_add(len)).unwrap_or(i64::MAX);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    c.count(
        "SELECT count(*) FROM bindings WHERE resource = ?1 AND kind = 'vb' \
         AND coalesce(offset, 0) < ?3 AND (size IS NULL OR coalesce(offset, 0) + size > ?2)",
        rusqlite::params![id, offset, end],
    ) > 0
}

/// The file a key list's name is part of: the name up to `#` (`m.flver#vb0` is part of `m.flver`).
fn file_part(name: &str) -> &str {
    name.split('#').next().unwrap_or(name)
}

#[derive(Default)]
pub struct Names {
    indexes: Vec<Index>,
    cache: RefCell<HashMap<i64, Option<Match>>>,
    /// Buffers' contents as the frame began, read once for the ranges named in them.
    contents: RefCell<HashMap<i64, Option<Rc<Vec<u8>>>>>,
}

impl Names {
    pub fn open(paths: &[PathBuf]) -> Result<Names, String> {
        Ok(Names {
            indexes: paths.iter().map(|p| Index::open(p)).collect::<Result<_, _>>()?,
            ..Names::default()
        })
    }

    /// What the `len` bytes at `offset` of buffer `id` (as the frame began) were made from, if an index knows: a
    /// mesh's vertex or index bytes in a buffer that holds more (a pool, or a file's data block padded when uploaded).
    pub fn range(&self, c: &Capture, id: i64, offset: u64, len: u64) -> Option<Match> {
        if self.indexes.is_empty() || len == 0 {
            return None;
        }
        let bytes = self.contents_of(c, id)?;
        let end = offset.checked_add(len)?;
        let part = bytes.get(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)?;
        let (file, others) = self.whole(&key(part), &Like::Buffer)?;
        Some(Match {
            file,
            how: format!("bytes {offset}..{end}"),
            others,
        })
    }

    /// What the bytes at `offset` of buffer `id`, bound as indices, were made from. Index lists alone name nothing: two
    /// triangles of a quad, or a grid's rows, are the same bytes in any engine's meshes. So the match must hold bytes the
    /// frame binds as vertices too (a FLVER's data block, indices and vertices in one), or vertex data bound with the
    /// indices (`vbs`: each vertex binding's resource, offset and size) must name the same file.
    pub fn indices(&self, c: &Capture, id: i64, offset: u64, len: u64, vbs: &[Vb]) -> Option<Match> {
        let m = self.range(c, id, offset, len)?;
        (holds_vertices(c, id, offset, len) || self.agrees(c, &m, vbs)).then_some(m)
    }

    /// Whether one of `vbs` is named from the same file as `m` (the names' part before `#`: a key list names the
    /// parts of a file `file#part`).
    fn agrees(&self, c: &Capture, m: &Match, vbs: &[Vb]) -> bool {
        vbs.iter().any(|&(r, offset, size)| {
            let named = self.of(c, r).or_else(|| {
                let (o, n) = (u64::try_from(offset?).ok()?, u64::try_from(size?).ok()?);
                self.range(c, r, o, n)
            });
            named.is_some_and(|v| file_part(&v.file.name) == file_part(&m.file.name))
        })
    }

    /// The vertex bindings of the draws that bind buffer `id` as indices (at `range`, or anywhere).
    pub fn vertices_beside(c: &Capture, id: i64, range: Option<(i64, i64)>) -> Vec<Vb> {
        let (offset, size) = range.unzip();
        c.rows(
            "SELECT DISTINCT v.resource, v.offset, v.size FROM bindings b JOIN bindings v ON v.draw = b.draw \
             AND v.kind = 'vb' AND v.resource IS NOT NULL WHERE b.resource = ?1 AND b.kind = 'ib' \
             AND (?2 IS NULL OR (b.offset = ?2 AND b.size = ?3))",
            rusqlite::params![id, offset, size],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }

    fn contents_of(&self, c: &Capture, id: i64) -> Option<Rc<Vec<u8>>> {
        self.contents
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| {
                let key: String = c
                    .row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
                    .flatten()?;
                c.blob(&key).ok().map(Rc::new)
            })
            .clone()
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    fn whole(&self, key: &str, like: &Like) -> Option<(File, usize)> {
        let mut found = None;
        let mut count = 0;
        for index in &self.indexes {
            let (files, n) = index.whole(key, like, 1);
            count += n;
            if found.is_none() {
                found = files.into_iter().next();
            }
        }
        found.map(|f| (f, count - 1))
    }

    /// What resource `id` of `c` was made from, if an index knows.
    pub fn of(&self, c: &Capture, id: i64) -> Option<Match> {
        if self.indexes.is_empty() {
            return None;
        }
        if let Some(m) = self.cache.borrow().get(&id) {
            return m.clone();
        }
        let mut m = self.find(c, id);
        // A buffer the frame binds only as indices is named only when its draws' vertex data agrees (see `indices`).
        if let Some(found) = &m
            && found.file.kind == "buffer"
            && !holds_vertices(c, id, 0, u64::MAX)
            && c.count(
                "SELECT count(*) FROM bindings WHERE resource = ?1 AND kind = 'ib'",
                [id],
            ) > 0
            && !self.agrees(c, found, &Names::vertices_beside(c, id, None))
        {
            m = None;
        }
        self.cache.borrow_mut().insert(id, m.clone());
        m
    }

    fn find(&self, c: &Capture, id: i64) -> Option<Match> {
        let (upload, content): (Option<String>, Option<String>) =
            c.row("SELECT upload, content FROM resources WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let s = shape(c, id)?;
        let like = if s.kind == "buffer" {
            Like::Buffer
        } else {
            Like::texture(s.format, s.width, s.height)
        };
        for (key, how) in [(&upload, "upload"), (&content, "contents")] {
            if let Some((file, others)) = key.as_deref().and_then(|k| self.whole(k, &like)) {
                return Some(Match {
                    file,
                    how: how.into(),
                    others,
                });
            }
        }
        // Subresource by subresource: a texture made from part of a file (fewer mips, one face).
        if s.kind == "buffer" {
            return None;
        }
        let subs: Vec<(i64, i64, i64, Option<String>, u32, u32)> = c.rows(
            "SELECT sub, mip, slice, hash, width, height FROM subresources WHERE resource = ?1 ORDER BY sub",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        );
        let total = subs.len();
        let mut matched: Vec<(i64, i64, File, u32, u32, usize)> = Vec::new();
        for (_, mip, slice, hash, width, height) in &subs {
            let Some(hash) = hash else { continue };
            let like = Like::texture(s.format, *width, *height);
            if let Some((s, n)) = self.indexes.iter().find_map(|i| i.sub(hash, &like)) {
                matched.push((*mip, *slice, s.file, s.mip, s.slice, n));
            }
        }
        let (mip, slice, file, fmip, fslice, n) = matched.first().cloned()?;
        let same_file = matched.iter().all(|m| m.2.name == file.name);
        let identical = matched
            .iter()
            .all(|m| m.0 == i64::from(m.3) && m.1 == i64::from(m.4));
        let how = if same_file && identical && matched.len() == total {
            "every subresource".to_string()
        } else if same_file {
            let mips: Vec<i64> = matched.iter().map(|m| m.0).collect();
            let (lo, hi) = (mips.iter().min()?, mips.iter().max()?);
            let shift = i64::from(fmip) - mip;
            let mut how = format!("{} of {total} subresources: mips {lo}..={hi}", matched.len());
            if shift != 0 {
                how += &format!(" are the file's mips {}..={}", lo + shift, hi + shift);
            }
            if slice != i64::from(fslice) {
                how += &format!(", slice {slice} its slice {fslice}");
            }
            how
        } else {
            format!(
                "{} of {total} subresources, from more than one file; mip {mip} slice {slice}",
                matched.len()
            )
        };
        Some(Match {
            file,
            how,
            others: n - 1,
        })
    }
}

/// Adds every DDS file under each of `inputs` (or the file itself) to the index at `out`, named by its path from the
/// input, and the buffers each of `keys` lists: lines of `key<TAB>bytes<TAB>name`, as a game's reader writes them for
/// what is inside its archives (fromsoft-formats' `flver_keys` for FLVER vertex and index bytes, unity-formats'
/// `mesh_keys` for a Unity player's Mesh vertex streams and index buffers).
pub fn index(out: &Path, inputs: &[PathBuf], keys: &[PathBuf]) -> Result<(), String> {
    let mut index = Index::create(out)?;
    let (mut added, mut refused) = (0usize, Vec::new());
    let mut keyed = 0usize;
    for list in keys {
        let text = std::fs::read_to_string(list).map_err(|e| format!("{}: {e}", list.display()))?;
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let at = format!("{}:{}", list.display(), n + 1);
            let mut parts = line.splitn(3, '\t');
            let (Some(key), Some(bytes), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
                refused.push(format!("{at}: not key<TAB>bytes<TAB>name"));
                continue;
            };
            let Ok(bytes) = bytes.parse::<u64>() else {
                refused.push(format!("{at}: {bytes:?} is not a count of bytes"));
                continue;
            };
            match index.add_buffer_key(name, bytes, key) {
                Ok(()) => keyed += 1,
                Err(e) => refused.push(format!("{at}: {e}")),
            }
            if keyed % 10000 == 9999 {
                index.commit()?;
            }
        }
    }
    for input in inputs {
        let mut stack = vec![input.clone()];
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                let mut entries: Vec<PathBuf> = std::fs::read_dir(&p)
                    .map_err(|e| format!("{}: {e}", p.display()))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .collect();
                entries.sort();
                stack.extend(entries.into_iter().rev());
                continue;
            }
            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dds")) {
                continue;
            }
            let name = p
                .strip_prefix(input)
                .ok()
                .filter(|r| !r.as_os_str().is_empty())
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            match index.add_dds(&name, &bytes) {
                Ok(()) => added += 1,
                Err(e) => refused.push(format!("{name}: {e}")),
            }
            if added % 1000 == 999 {
                index.commit()?;
            }
        }
    }
    index.commit()?;
    out!(
        "{added} DDS files and {keyed} buffer keys added to {}",
        out.display()
    );
    if !refused.is_empty() {
        out!("{} refused:", refused.len());
        for r in refused.iter().take(20) {
            out!("  {r}");
        }
        if refused.len() > 20 {
            out!("  ... {} more", refused.len() - 20);
        }
    }
    let counts: Vec<String> = index
        .counts()
        .into_iter()
        .map(|(k, n)| format!("{n} {k}s"))
        .collect();
    out!("the index holds {}", counts.join(", "));
    Ok(())
}

/// The distinct (offset, size) ranges the frame's draws bound buffer `id` at as vertices or indices, each with what
/// an index names it.
fn bound_ranges(c: &Capture, id: i64) -> Vec<((i64, i64), Option<Match>)> {
    // Each range once, with whether any draw binds it as indices.
    let ranges: Vec<(bool, i64, i64)> = c.rows(
        "SELECT max(kind = 'ib'), offset, size FROM bindings WHERE resource = ?1 AND kind IN ('vb', 'ib') \
         AND offset IS NOT NULL AND size > 0 GROUP BY offset, size ORDER BY offset, size",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    ranges
        .into_iter()
        .map(|(indices, o, n)| {
            let m = if indices {
                let vbs = Names::vertices_beside(c, id, Some((o, n)));
                c.names.indices(c, id, o as u64, n as u64, &vbs)
            } else {
                c.names.range(c, id, o as u64, n as u64)
            };
            ((o, n), m)
        })
        .collect()
}

/// What each resource of `c` was made from: those the frame used (but its targets), or `all`.
pub fn join(c: &Capture, all: bool, unnamed: bool, limit: usize) -> Result<(), String> {
    if c.names.is_empty() {
        return Err("no index: give --index INDEX.db (or set LOCUM_INDEX)".into());
    }
    let ids: Vec<(i64, String, i64)> = c.rows(
        &format!(
            "SELECT r.id, r.kind, (SELECT count(DISTINCT draw) FROM bindings b WHERE b.resource = r.id) \
             FROM resources r WHERE (r.kind != 'buffer' OR r.bind & 0x4 = 0) {} ORDER BY r.id",
            // Constant buffers are never files; render and depth targets are what the frame makes.
            if all {
                ""
            } else {
                "AND r.used = 1 AND r.bind & 0x60 = 0"
            }
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    // Named, not named, and not named for want of bytes (neither upload bytes nor contents recorded), by kind.
    let mut tally: HashMap<&str, (usize, usize, usize)> = HashMap::new();
    let mut shown = 0;
    for (id, kind, draws) in &ids {
        let k = if kind == "buffer" { "buffer" } else { "texture" };
        let m = c.names.of(c, *id);
        // A buffer no file is whole: the vertex and index ranges the draws bound in it, each by its own file.
        let ranges = if m.is_none() && k == "buffer" {
            bound_ranges(c, *id)
        } else {
            Vec::new()
        };
        let named = m.is_some() || ranges.iter().any(|r| r.1.is_some());
        let t = tally.entry(k).or_default();
        if named {
            t.0 += 1;
        } else {
            t.1 += 1;
            let bytes = c
                .row(
                    "SELECT upload IS NOT NULL OR content IS NOT NULL FROM resources WHERE id = ?1",
                    [*id],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false);
            if !bytes {
                t.2 += 1;
            }
        }
        if named == unnamed || shown >= limit {
            continue;
        }
        shown += 1;
        let s = shape(c, *id).map_or(format!("#{id}"), |s| s.to_string());
        let used = if *draws > 0 {
            format!(", bound by {draws} draws")
        } else {
            String::new()
        };
        match m {
            Some(m) => out!("{s}{used}\n    {m}"),
            None => out!("{s}{used}"),
        }
        let found: Vec<&Match> = ranges.iter().filter_map(|r| r.1.as_ref()).collect();
        if !ranges.is_empty() && !found.is_empty() {
            out!("    {} of {} bound ranges named:", found.len(), ranges.len());
            for m in found.iter().take(8) {
                out!("    {m}");
            }
            if found.len() > 8 {
                out!("    ... {} more", found.len() - 8);
            }
        }
    }
    let what = if all { "alive" } else { "the frame used" };
    for k in ["texture", "buffer"] {
        if let Some((named, not, bytes_missing)) = tally.get(k) {
            let missing = match bytes_missing {
                0 => String::new(),
                n if n == not => format!(" (none of the {n} others has bytes recorded to match)"),
                n => format!(" ({n} of the {not} others have no bytes recorded to match)"),
            };
            out!("{k}s {what}: {named} of {} named{missing}", named + not);
        }
    }
    if shown >= limit {
        out!("(at most {limit} listed: --limit)");
    }
    Ok(())
}
