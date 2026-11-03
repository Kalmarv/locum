//! Words for what is in a capture: resources, stacks, constants by name.

use std::collections::HashMap;

use locum_capture::format;
use locum_capture::rdef::{self, ConstantBuffer};

use crate::open::Capture;

/// Prints a line to standard output, and leaves quietly when it is closed (piped into `head`).
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

/// The order bindings are listed in: input assembly, the shader stages, stream output, output merger.
pub const STAGE_ORDER: &str = "CASE stage WHEN 'ia' THEN 0 WHEN 'vs' THEN 1 WHEN 'hs' THEN 2 WHEN 'ds' THEN 3 \
     WHEN 'gs' THEN 4 WHEN 'so' THEN 5 WHEN 'ps' THEN 6 WHEN 'cs' THEN 7 ELSE 8 END";

/// A resource's shape as the `resources` table has it.
#[derive(Clone, Debug, Default)]
pub struct Shape {
    pub id: i64,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub samples: u32,
    pub bytes: u64,
    pub stride: u32,
    pub misc: u32,
}

pub const SHAPE_COLUMNS: &str =
    "id, kind, width, height, depth, mips, array, format, samples, bytes, stride, misc";

impl Shape {
    /// From a row whose first columns are [`SHAPE_COLUMNS`].
    pub fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Shape> {
        let n = |i: usize| -> rusqlite::Result<u32> { Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0) as u32) };
        Ok(Shape {
            id: r.get(0)?,
            kind: r.get(1)?,
            width: n(2)?,
            height: n(3)?,
            depth: n(4)?,
            mips: n(5)?,
            array: n(6)?,
            format: n(7)?,
            samples: n(8)?,
            bytes: r.get::<_, i64>(9)? as u64,
            stride: n(10)?,
            misc: n(11)?,
        })
    }

    pub fn cube(&self) -> bool {
        self.misc & 4 != 0
    }

    /// The subresources of its contents in the canonical layout (as locum laid them out).
    pub fn subs(&self) -> Vec<locum_capture::layout::Sub> {
        if self.kind == "buffer" {
            return locum_capture::layout::buffer(self.width);
        }
        locum_capture::layout::texture(
            self.format,
            self.width,
            self.height,
            self.depth,
            self.mips,
            self.array,
        )
        .unwrap_or_default()
    }
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind.as_str() {
            "buffer" => {
                write!(f, "#{} buffer {} bytes", self.id, self.width)?;
                if self.stride > 0 {
                    write!(f, " stride {}", self.stride)?;
                }
                Ok(())
            }
            kind => {
                write!(f, "#{} {kind} {}", self.id, self.width)?;
                if kind != "texture1d" {
                    write!(f, "x{}", self.height)?;
                }
                if kind == "texture3d" {
                    write!(f, "x{}", self.depth)?;
                }
                write!(f, " {}", format::name(self.format))?;
                if self.mips > 1 {
                    write!(f, " {} mips", self.mips)?;
                }
                if self.cube() {
                    write!(f, " cube")?;
                    if self.array > 6 {
                        write!(f, " x{}", self.array / 6)?;
                    }
                } else if self.array > 1 {
                    write!(f, " [{}]", self.array)?;
                }
                if self.samples > 1 {
                    write!(f, " {}x MSAA", self.samples)?;
                }
                Ok(())
            }
        }
    }
}

pub fn shape(c: &Capture, id: i64) -> Option<Shape> {
    c.row(
        &format!("SELECT {SHAPE_COLUMNS} FROM resources WHERE id = ?1"),
        [id],
        Shape::from_row,
    )
}

/// A stack's frames as `module+0xoffset`, innermost first.
pub fn stack(c: &Capture, id: i64) -> Vec<String> {
    let Some(frames) = c.row("SELECT frames FROM stacks WHERE id = ?1", [id], |r| {
        r.get::<_, Vec<u8>>(0)
    }) else {
        return Vec::new();
    };
    let modules: Vec<(String, u64, u64)> = c.rows("SELECT path, base, size FROM modules", [], |r| {
        Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64))
    });
    frames
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&b| {
            let a = u64::from_le_bytes(b);
            match modules
                .iter()
                .find(|(_, base, size)| (*base..base + size).contains(&a))
            {
                Some((path, base, _)) => {
                    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
                    format!("{name}+0x{:x}", a - base)
                }
                None => format!("0x{a:x}"),
            }
        })
        .collect()
}

/// Shader reflection by shader hash, read once.
#[derive(Default)]
pub struct Reflection {
    cache: HashMap<String, Option<Vec<ConstantBuffer>>>,
}

impl Reflection {
    /// The constant buffer a shader declares at register `slot` (space `space`).
    pub fn cb<'a>(
        &'a mut self,
        c: &Capture,
        shader: &str,
        slot: u32,
        space: u32,
    ) -> Option<&'a ConstantBuffer> {
        let cbs = self.cache.entry(shader.to_string()).or_insert_with(|| {
            let key: String = c.row(
                "SELECT blob FROM shaders WHERE hash = ?1 AND blob IS NOT NULL",
                [shader],
                |r| r.get(0),
            )?;
            rdef::constant_buffers(&c.blob(&key).ok()?)
        });
        cbs.as_ref()?
            .iter()
            .find(|b| b.slot == Some(slot) && b.space == space)
    }
}

/// A 4-byte constant as its type says (float when unknown), at most 12 characters.
pub fn value(bytes: [u8; 4], base: Option<u16>) -> String {
    match base {
        Some(1) => (u32::from_le_bytes(bytes) != 0).to_string(),
        Some(2) => i32::from_le_bytes(bytes).to_string(),
        Some(19 | 20) => u32::from_le_bytes(bytes).to_string(),
        _ => float(f32::from_le_bytes(bytes)),
    }
}

/// A float in at most 12 characters: plainly when it fits, else in exponent form; denormals and NaNs (more likely
/// integers or packed bits) as hex.
pub fn float(f: f32) -> String {
    let bits = f.to_bits();
    if (f != 0.0 && f.abs() < 1e-30) || f.is_nan() {
        return format!("0x{bits:08x}");
    }
    let a = f.abs();
    let plain = format!("{f}");
    if plain.len() <= 12 && a < 1e7 {
        return plain;
    }
    if (1e-4..1e7).contains(&a) {
        // Round to fit: the integer digits, the point and as many decimals as are left.
        let digits = (a.log10().floor() as i32 + 1).max(1) as usize;
        let sign = usize::from(f < 0.0);
        let decimals = 12usize.saturating_sub(digits + 1 + sign).min(9);
        return format!("{f:.decimals$}");
    }
    format!("{f:.4e}")
}

/// The names of a register's four components, shortened to `name` when they are `name.x` to `name.w`.
pub fn register_name(names: &[Option<String>; 4]) -> String {
    if let Some(Some(first)) = names.first()
        && let Some(base) = first.strip_suffix(".x")
        && names
            .iter()
            .zip(["x", "y", "z", "w"])
            .all(|(n, c)| n.as_deref() == Some(&format!("{base}.{c}")))
    {
        return base.to_string();
    }
    let mut distinct: Vec<&str> = Vec::new();
    for n in names.iter().flatten() {
        if !distinct.contains(&n.as_str()) {
            distinct.push(n);
        }
    }
    distinct.join(" ")
}

/// Short key for tables.
pub fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}

/// A count of bytes in MiB with one decimal.
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

/// A draw's or dispatch's arguments by name (`count=714 start=885904 base=0`): the API puts the start index in a
/// different place in each call. Indirect calls name their argument buffer. Arguments of another shape are shown as
/// recorded.
pub fn call_args(call: &str, args: &str) -> String {
    let names: &[&str] = match call {
        "Draw" => &["count", "start"],
        "DrawIndexed" => &["count", "start", "base"],
        "DrawInstanced" => &["count", "instances", "start", "first_instance"],
        "DrawIndexedInstanced" => &["count", "instances", "start", "base", "first_instance"],
        "Dispatch" => &["x", "y", "z"],
        "DrawInstancedIndirect" | "DrawIndexedInstancedIndirect" | "DispatchIndirect" => &["args", "offset"],
        _ => &[],
    };
    let values: Vec<i64> = serde_json::from_str(args).unwrap_or_default();
    if names.is_empty() || values.len() != names.len() {
        return args.to_string();
    }
    names
        .iter()
        .zip(&values)
        .map(|(name, v)| match *name {
            "args" => format!("args=#{v}"),
            _ => format!("{name}={v}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}
