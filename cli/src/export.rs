//! `export`, `target` and `blob`: contents out of the store as DDS, PNG or raw bytes.

use std::path::Path;

use locum_capture::dds::{self, Texture};
use locum_capture::format;
use locum_capture::texels::{self, Range};

use crate::describe::shape;
use crate::open::Capture;
use crate::out;

/// What to write, from the output file's extension.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dds,
    Png,
    Raw,
}

fn kind_of(out: &Path) -> Kind {
    match out
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("dds") => Kind::Dds,
        Some("png") => Kind::Png,
        _ => Kind::Raw,
    }
}

pub fn png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

pub struct Which {
    pub sub: Option<u32>,
    pub mip: u32,
    pub slice: u32,
    /// The depth slice of a volume, for PNG.
    pub z: u32,
    /// The upload instead of the contents as the frame began.
    pub upload: bool,
}

pub fn export(c: &Capture, id: i64, out_path: &Path, which: &Which, range: Range) -> Result<(), String> {
    let s = shape(c, id).ok_or_else(|| format!("no resource #{id}"))?;
    let column = if which.upload { "upload" } else { "content" };
    let key: String = c
        .row(
            &format!("SELECT {column} FROM resources WHERE id = ?1"),
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .flatten()
        .ok_or_else(|| format!("#{id} has no {column} in this capture"))?;
    let bytes = c.blob(&key)?;
    let subs = s.subs();
    if subs.is_empty() {
        return Err(format!("{} has no layout locum knows", format::name(s.format)));
    }
    let total = locum_capture::layout::total(&subs);
    if bytes.len() as u64 != total {
        return Err(format!(
            "blob {key} has {} bytes, {} expected for {s}",
            bytes.len(),
            total
        ));
    }
    let kind = kind_of(out_path);
    if s.kind == "buffer" && kind != Kind::Raw {
        return Err("a buffer exports as raw bytes (give a file name without .dds or .png)".into());
    }
    let pick = || -> Result<locum_capture::layout::Sub, String> {
        let index = which.sub.unwrap_or(which.mip + which.slice * s.mips.max(1));
        subs.iter()
            .find(|x| x.index == index)
            .copied()
            .ok_or_else(|| format!("{s} has no subresource {index}"))
    };
    let readable = format::readable(s.format);
    match kind {
        Kind::Raw => {
            let (from, len) = if which.sub.is_some() || which.mip > 0 || which.slice > 0 {
                let sub = pick()?;
                (sub.offset as usize, sub.size as usize)
            } else {
                (0, bytes.len())
            };
            write(out_path, &bytes[from..from + len])?;
            out!("{}: {len} bytes of #{id} ({key})", out_path.display());
        }
        Kind::Dds => {
            let whole = which.sub.is_none() && which.mip == 0 && which.slice == 0;
            let dimensions = match s.kind.as_str() {
                "texture1d" => 1,
                "texture3d" => 3,
                _ => 2,
            };
            let file = if whole {
                dds::file(
                    &Texture {
                        format: readable,
                        dimensions,
                        width: s.width,
                        height: s.height,
                        depth: s.depth,
                        mips: s.mips,
                        array: s.array,
                        cube: s.cube(),
                    },
                    &bytes,
                )
            } else {
                let sub = pick()?;
                dds::file(
                    &Texture {
                        format: readable,
                        dimensions,
                        width: sub.width,
                        height: sub.height,
                        depth: sub.depth,
                        mips: 1,
                        array: 1,
                        cube: false,
                    },
                    &bytes[sub.offset as usize..(sub.offset + sub.size) as usize],
                )
            };
            write(out_path, &file)?;
            if whole {
                out!("{}: {s} as {}", out_path.display(), format::name(readable));
            } else {
                let sub = pick()?;
                out!(
                    "{}: #{id} mip {} slice {} {}x{} as {}",
                    out_path.display(),
                    sub.mip,
                    sub.slice,
                    sub.width,
                    sub.height,
                    format::name(readable)
                );
            }
        }
        Kind::Png => {
            let sub = pick()?;
            let z = which.z.min(sub.depth.saturating_sub(1));
            let start = sub.offset + sub.slice_bytes() * u64::from(z);
            let image = texels::decode(
                s.format,
                sub.width,
                sub.height,
                &bytes[start as usize..(start + sub.slice_bytes()) as usize],
            )?;
            png(out_path, sub.width, sub.height, &image.rgba8(range))?;
            out!(
                "{}: #{id} mip {} slice {}{} {}x{} {}",
                out_path.display(),
                sub.mip,
                sub.slice,
                if sub.depth > 1 {
                    format!(" z {z}")
                } else {
                    String::new()
                },
                sub.width,
                sub.height,
                format::name(s.format)
            );
        }
    }
    Ok(())
}

pub fn target(c: &Capture, draw: i64, name: &str, out_path: &Path, range: Range) -> Result<(), String> {
    let (key, w, h, f): (Option<String>, i64, i64, i64) = c
        .row(
            "SELECT blob, width, height, format FROM targets WHERE draw = ?1 AND target = ?2",
            rusqlite::params![draw, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok_or_else(|| {
            let read: Vec<String> = c.rows(
                "SELECT draw || ' ' || target FROM targets ORDER BY draw",
                [],
                |r| r.get(0),
            );
            format!(
                "no {name} read back after draw {draw}; targets are read after each pass's last draw: {}",
                read.join(", ")
            )
        })?;
    let key = key.ok_or("the target was not stored")?;
    let bytes = c.blob(&key)?;
    let (w, h, f) = (w as u32, h as u32, f as u32);
    match kind_of(out_path) {
        Kind::Png => {
            let image = texels::decode(f, w, h, &bytes)?;
            png(out_path, w, h, &image.rgba8(range))?;
        }
        Kind::Dds => {
            let t = Texture {
                format: f,
                dimensions: 2,
                width: w,
                height: h,
                depth: 1,
                mips: 1,
                array: 1,
                cube: false,
            };
            write(out_path, &dds::file(&t, &bytes))?;
        }
        Kind::Raw => write(out_path, &bytes)?,
    }
    out!(
        "{}: {name} after draw {draw}, {w}x{h} {}",
        out_path.display(),
        format::name(f)
    );
    Ok(())
}

pub fn blob(c: &Capture, key: &str, out_path: &Path) -> Result<(), String> {
    let bytes = c.blob(key)?;
    write(out_path, &bytes)?;
    out!("{}: {} bytes", out_path.display(), bytes.len());
    Ok(())
}
