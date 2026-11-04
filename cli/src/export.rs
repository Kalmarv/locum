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
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    write(path, &file)
}

/// Whether `path` is `-`: the bytes go to stdout, and what they are to stderr.
fn stdout(path: &Path) -> bool {
    path.as_os_str() == "-"
}

/// Writes `bytes` to `path` whole or not at all: into a file beside it, renamed over it once written, so a failed
/// write (a full disk) leaves no partial file. `-` writes them to stdout.
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    if stdout(path) {
        let mut out = std::io::stdout().lock();
        return out
            .write_all(bytes)
            .and_then(|()| out.flush())
            .map_err(|e| format!("stdout: {e}"));
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    std::fs::write(&partial, bytes)
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            format!("{}: {e}", path.display())
        })
}

/// `--quiet`: say nothing of what was written, only warnings.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What was written where: on stderr when the bytes went to stdout; with `--quiet`, warnings only.
macro_rules! told {
    ($path:expr, $($arg:tt)*) => {{
        let line = format!($($arg)*);
        if !QUIET.load(std::sync::atomic::Ordering::Relaxed) || line.starts_with("warning") {
            if stdout($path) {
                eprintln!("{line}");
            } else {
                out!("{line}");
            }
        }
    }};
}

/// How a PNG is drawn: the value mapping, one channel as grey, and a part of the image.
pub struct Picture {
    pub range: Range,
    /// 0 to 3: r, g, b, a.
    pub channel: Option<usize>,
    /// x, y, width, height.
    pub rect: Option<[u32; 4]>,
    /// To the smallest rectangle holding every texel that is not zero.
    pub crop: bool,
    /// The alpha channel as the PNG's alpha; otherwise the picture is opaque.
    pub alpha: bool,
}

impl Picture {
    /// `image` as asked, and what was done to it: the channel, the part, and the mapping to 0..255.
    fn apply(&self, image: texels::Image) -> Result<(texels::Image, String), String> {
        let mut note = Vec::new();
        let mut image = match self.channel {
            Some(c) if c >= usize::from(image.channels.max(1)) => {
                return Err(format!(
                    "the format has {} channel(s): no channel {}",
                    image.channels,
                    ["r", "g", "b", "a"][c]
                ));
            }
            Some(c) => {
                note.push(format!("channel {} as grey", ["r", "g", "b", "a"][c]));
                image.channel(c)
            }
            None => image,
        };
        let rect = match (self.rect, self.crop) {
            (Some(r), _) => Some(r),
            (None, true) => Some(image.content().ok_or("every texel is zero: nothing to crop to")?),
            (None, false) => None,
        };
        if let Some(r) = rect {
            image = image.crop(r);
            note.push(format!(
                "texels {},{} {}x{}",
                r[0], r[1], image.width, image.height
            ));
        }
        note.push(match image.mapping(self.range) {
            Range::Fixed(lo, hi) => format!("{lo}..{hi} as 0..255"),
            Range::Tonemap => "x/(1+x) as 0..255".into(),
            _ => "0..1 as 0..255".into(),
        });
        if self.alpha && image.channels == 4 {
            note.push("alpha as the PNG's alpha".into());
        }
        Ok((image, note.join(", ")))
    }

    /// The PNG's pixels: opaque unless the alpha is kept (a target's alpha is rarely coverage, and a zero there
    /// would hide the colour).
    fn rgba8(&self, image: &texels::Image) -> Vec<u8> {
        let mut rgba = image.rgba8(self.range);
        if !self.alpha {
            for a in rgba.iter_mut().skip(3).step_by(4) {
                *a = 255;
            }
        }
        rgba
    }
}

pub struct Which {
    pub sub: Option<u32>,
    pub mip: u32,
    pub slice: u32,
    /// The depth slice of a volume, for PNG.
    pub z: u32,
    /// The upload instead of the contents as the frame began.
    pub upload: bool,
    /// The contents just before this event instead, replayed (on WARP with `warp`).
    pub at: Option<i64>,
    pub warp: bool,
    /// Of a buffer, only bytes `a..b`.
    pub bytes: Option<(i64, i64)>,
}

pub fn export(c: &Capture, id: i64, out_path: &Path, which: &Which, picture: &Picture) -> Result<(), String> {
    let s = shape(c, id).ok_or_else(|| format!("no resource #{id}"))?;
    let (bytes, key) = match which.at {
        Some(event) => {
            let (bytes, events, warnings) = crate::replay::contents_at(c, id, event, which.warp)?;
            for w in &warnings {
                told!(out_path, "warning: {w}");
            }
            let source =
                format!("as event {event} saw it: replayed from the frame's start with {events} events");
            told!(out_path, "#{id} {source}");
            // The write it holds last: a discarding map that wrote little or nothing leaves locum's pattern.
            let last: Option<(i64, String, Option<String>)> = c.row(
                "SELECT e.seq, e.call, e.args FROM events e JOIN event_resources r ON r.event = e.seq \
                 WHERE r.resource = ?1 AND r.access = 'write' AND e.seq < ?2 ORDER BY e.seq DESC LIMIT 1",
                rusqlite::params![id, event],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            );
            if let Some((seq, call, args)) = last
                && let Some(d) = crate::resources::discarded(c, &call, args.as_deref())
            {
                told!(out_path, "warning: the map at event {seq} {d}");
            }
            (bytes, source)
        }
        None => {
            let column = if which.upload { "upload" } else { "content" };
            let key: String = c
                .row(
                    &format!("SELECT {column} FROM resources WHERE id = ?1"),
                    [id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .flatten()
                .ok_or_else(|| format!("#{id} has no {column} in this capture"))?;
            (c.blob(&key)?, key)
        }
    };
    let subs = s.subs();
    if subs.is_empty() {
        return Err(format!("{} has no layout locum knows", format::name(s.format)));
    }
    let total = locum_capture::layout::total(&subs);
    if bytes.len() as u64 != total {
        return Err(format!(
            "contents ({key}) have {} bytes, {} expected for {s}",
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
            let (from, len) = if let Some((a, b)) = which.bytes {
                if s.kind != "buffer" {
                    return Err("--bytes is for buffers; a texture's parts are --sub, --mip, --slice".into());
                }
                let (a, b) = (a.max(0) as usize, (b.max(0) as usize).min(bytes.len()));
                if a >= b {
                    return Err(format!(
                        "--bytes {a}..{b}: nothing of #{id}'s {} bytes",
                        bytes.len()
                    ));
                }
                (a, b - a)
            } else if which.sub.is_some() || which.mip > 0 || which.slice > 0 {
                let sub = pick()?;
                (sub.offset as usize, sub.size as usize)
            } else {
                (0, bytes.len())
            };
            write(out_path, &bytes[from..from + len])?;
            if s.kind == "buffer" {
                told!(out_path, "{}: {len} bytes of #{id} ({key})", out_path.display());
            } else {
                // What the bytes are: subresources in order, each mip's rows tightly packed.
                let texel = format::texel(s.format)
                    .map(|t| format!("; {t}"))
                    .unwrap_or_default();
                told!(
                    out_path,
                    "{}: {len} bytes ({key}) of {s}, rows tightly packed{texel}",
                    out_path.display()
                );
            }
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
                told!(
                    out_path,
                    "{}: {s} as {}",
                    out_path.display(),
                    format::name(readable)
                );
            } else {
                let sub = pick()?;
                told!(
                    out_path,
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
            let (image, note) = picture.apply(image)?;
            png(out_path, image.width, image.height, &picture.rgba8(&image))?;
            told!(
                out_path,
                "{}: #{id} mip {} slice {}{} {}x{} {} ({note})",
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

pub fn target(c: &Capture, draw: i64, name: &str, out_path: &Path, picture: &Picture) -> Result<(), String> {
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
    let mut note = String::new();
    match kind_of(out_path) {
        Kind::Png => {
            let (image, n) = picture.apply(texels::decode(f, w, h, &bytes)?)?;
            png(out_path, image.width, image.height, &picture.rgba8(&image))?;
            note = format!(" ({n})");
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
    told!(
        out_path,
        "{}: {name} after draw {draw}, {w}x{h} {}{note}",
        out_path.display(),
        format::name(f)
    );
    Ok(())
}

pub fn blob(c: &Capture, key: &str, out_path: &Path) -> Result<(), String> {
    let bytes = c.blob(key)?;
    write(out_path, &bytes)?;
    told!(out_path, "{}: {} bytes", out_path.display(), bytes.len());
    Ok(())
}
