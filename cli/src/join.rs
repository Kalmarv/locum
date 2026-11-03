//! Names for a capture's resources from indexes of files (`locum_capture::index`): `index` keys loose DDS files,
//! `join` lists what each resource was made from, and every command that shows a bound resource shows its name when
//! an index is given (`--index`, or `LOCUM_INDEX` with paths separated by `;`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use locum_capture::index::{File, Index};

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

#[derive(Default)]
pub struct Names {
    indexes: Vec<Index>,
    cache: RefCell<HashMap<i64, Option<Match>>>,
}

impl Names {
    pub fn open(paths: &[PathBuf]) -> Result<Names, String> {
        Ok(Names {
            indexes: paths.iter().map(|p| Index::open(p)).collect::<Result<_, _>>()?,
            cache: RefCell::default(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    fn whole(&self, key: &str) -> Option<(File, usize)> {
        let mut found = None;
        let mut count = 0;
        for index in &self.indexes {
            let (files, n) = index.whole(key, 1);
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
        let m = self.find(c, id);
        self.cache.borrow_mut().insert(id, m.clone());
        m
    }

    fn find(&self, c: &Capture, id: i64) -> Option<Match> {
        let (upload, content): (Option<String>, Option<String>) =
            c.row("SELECT upload, content FROM resources WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        for (key, how) in [(&upload, "upload"), (&content, "contents")] {
            if let Some((file, others)) = key.as_deref().and_then(|k| self.whole(k)) {
                return Some(Match {
                    file,
                    how: how.into(),
                    others,
                });
            }
        }
        // Subresource by subresource: a texture made from part of a file (fewer mips, one face).
        let subs: Vec<(i64, i64, i64, Option<String>)> = c.rows(
            "SELECT sub, mip, slice, hash FROM subresources WHERE resource = ?1 ORDER BY sub",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );
        let total = subs.len();
        let mut matched: Vec<(i64, i64, File, u32, u32, usize)> = Vec::new();
        for (_, mip, slice, hash) in &subs {
            let Some(hash) = hash else { continue };
            if let Some((s, n)) = self.indexes.iter().find_map(|i| i.sub(hash)) {
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
/// input.
pub fn index(out: &Path, inputs: &[PathBuf]) -> Result<(), String> {
    let mut index = Index::create(out)?;
    let (mut added, mut refused) = (0usize, Vec::new());
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
    out!("{added} DDS files added to {}", out.display());
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
    // Named and not, by kind: textures, buffers.
    let mut tally: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut shown = 0;
    for (id, kind, draws) in &ids {
        let k = if kind == "buffer" { "buffer" } else { "texture" };
        let m = c.names.of(c, *id);
        let t = tally.entry(k).or_default();
        if m.is_some() {
            t.0 += 1;
        } else {
            t.1 += 1;
        }
        if m.is_some() == unnamed || shown >= limit {
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
    }
    let what = if all { "alive" } else { "the frame used" };
    for k in ["texture", "buffer"] {
        if let Some((named, not)) = tally.get(k) {
            out!("{k}s {what}: {named} of {} named", named + not);
        }
    }
    if shown >= limit {
        out!("(at most {limit} listed: --limit)");
    }
    Ok(())
}
