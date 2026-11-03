//! locum-cli: reads the captures locum writes. A capture is named by its `capture.db`, the folder holding it, its
//! name under locum's folder (`LOCUM_DIR`, or `%LOCALAPPDATA%\locum`), or `last`.

mod describe;
mod draws;
mod export;
mod info;
mod open;
mod resources;
mod shaders;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use locum_capture::texels::Range;

#[derive(Parser)]
#[command(name = "locum-cli", version, about = "Reads locum's captures of a frame")]
struct Cli {
    /// The blob store (default: LOCUM_BLOBS, or the one the capture names).
    #[arg(long, global = true)]
    blobs: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The captures in locum's folder, newest first.
    Captures {
        /// locum's folder (default LOCUM_DIR or %LOCALAPPDATA%\locum).
        dir: Option<PathBuf>,
    },
    /// What a capture holds, and what it is missing.
    Info { capture: String },
    /// The draws and dispatches, in the order the GPU ran them.
    Draws {
        capture: String,
        /// Vertex shader hash (or its start).
        #[arg(long)]
        vs: Option<String>,
        #[arg(long)]
        ps: Option<String>,
        #[arg(long)]
        cs: Option<String>,
        /// A shader at any stage.
        #[arg(long)]
        shader: Option<String>,
        #[arg(long)]
        pass: Option<i64>,
        /// Draws with this resource bound (any stage, any kind).
        #[arg(long)]
        uses: Option<i64>,
        /// Draws rendering to this resource.
        #[arg(long)]
        target: Option<i64>,
        /// Draws with a constant buffer, or a resource, whose contents or upload have this key (or its start).
        #[arg(long)]
        content: Option<String>,
        /// The call (Draw, DrawIndexed, Dispatch, ...; % matches anything).
        #[arg(long)]
        call: Option<String>,
    },
    /// One draw: its shaders, state, everything bound, its constants by name, and the targets read after it.
    Draw {
        capture: String,
        n: i64,
        /// Every constant register, not only those the shader uses.
        #[arg(long)]
        all: bool,
    },
    /// A draw's constant buffer, every register, with names from the shader.
    Cb {
        capture: String,
        draw: i64,
        /// vs, hs, ds, gs, ps or cs.
        stage: String,
        slot: u32,
        /// Hex bytes instead.
        #[arg(long)]
        raw: bool,
    },
    /// Where a value sits in the frame's constants: each buffer, register, name and the draws that saw it.
    Find {
        capture: String,
        #[arg(allow_negative_numbers = true)]
        value: f64,
        /// Relative tolerance for floats.
        #[arg(long, default_value_t = 1e-4)]
        tolerance: f64,
        /// Match 32-bit integers instead of floats.
        #[arg(long)]
        int: bool,
    },
    /// Passes: runs of draws with the same targets.
    Passes { capture: String },
    /// The resources alive at the capture.
    Resources {
        capture: String,
        /// Only those the frame used.
        #[arg(long)]
        used: bool,
        /// Only those the frame wrote.
        #[arg(long)]
        written: bool,
        /// buffer, texture1d, texture2d or texture3d.
        #[arg(long)]
        kind: Option<String>,
        /// A DXGI format name (BC7_UNORM, R16G16B16A16_FLOAT, ...).
        #[arg(long)]
        format: Option<String>,
        #[arg(long)]
        width: Option<i64>,
        #[arg(long)]
        height: Option<i64>,
        /// Contents, upload or subresource key (or its start).
        #[arg(long)]
        hash: Option<String>,
    },
    /// One resource: its description, where it was made, its contents and hashes, and what the frame did with it.
    Resource { capture: String, id: i64 },
    /// The timeline: every draw, dispatch, clear, copy, update and map.
    Events {
        capture: String,
        /// draw, dispatch, clear, copy, update, map, resolve, mips, ...
        #[arg(long)]
        kind: Option<String>,
        /// Events that read or wrote this resource.
        #[arg(long)]
        resource: Option<i64>,
        /// The first event to list.
        #[arg(long, default_value_t = 0)]
        from: i64,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// A call stack, as module+offset.
    Stack { capture: String, id: i64 },
    /// The shaders the frame used, and how many draws used each.
    Shaders { capture: String },
    /// A shader's constant buffers and, with --disassemble, its code.
    Shader {
        capture: String,
        hash: String,
        #[arg(long, short)]
        disassemble: bool,
    },
    /// A resource's contents as the frame began: .dds (whole, or one subresource), .png (one subresource), or raw.
    Export {
        capture: String,
        id: i64,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        sub: Option<u32>,
        #[arg(long, default_value_t = 0)]
        mip: u32,
        #[arg(long, default_value_t = 0)]
        slice: u32,
        /// The depth slice of a volume texture (for .png).
        #[arg(long, default_value_t = 0)]
        z: u32,
        /// The data it was created with instead.
        #[arg(long)]
        upload: bool,
        /// PNG values: auto, clamp, tonemap or LO:HI.
        #[arg(long, default_value = "auto")]
        range: String,
    },
    /// A target read back after a pass's last draw, as .png, .dds or raw.
    Target {
        capture: String,
        draw: i64,
        /// rt0 to rt7, or depth.
        #[arg(default_value = "rt0")]
        name: String,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long, default_value = "auto")]
        range: String,
    },
    /// A blob from the store, by key.
    Blob {
        capture: String,
        key: String,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Any SQL on the capture; rows tab-separated.
    Sql { capture: String, query: String },
}

fn range(s: &str) -> Result<Range, String> {
    Range::parse(s).ok_or_else(|| format!("--range {s}: auto, clamp, tonemap or LO:HI"))
}

fn sql(c: &open::Capture, query: &str) -> Result<(), String> {
    let mut q = c.db.prepare(query).map_err(|e| e.to_string())?;
    let n = q.column_count();
    out!("{}", q.column_names().collect::<Vec<_>>().join("\t"));
    let mut rows = q.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let cells: Vec<String> = (0..n)
            .map(|i| match r.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => "NULL".into(),
                Ok(rusqlite::types::ValueRef::Integer(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Real(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
                Ok(rusqlite::types::ValueRef::Blob(b)) => {
                    let hex: String = b.iter().take(64).map(|x| format!("{x:02x}")).collect();
                    if b.len() > 64 {
                        format!("{hex}... ({} bytes)", b.len())
                    } else {
                        hex
                    }
                }
                Err(e) => e.to_string(),
            })
            .collect();
        out!("{}", cells.join("\t"));
    }
    Ok(())
}

fn run(cli: Cli) -> Result<(), String> {
    let blobs = cli.blobs.as_deref();
    let open = |name: &str| open::open(name, blobs);
    match cli.command {
        Command::Captures { dir } => info::captures(&dir.unwrap_or_else(open::locum_dir), blobs),
        Command::Info { capture } => info::info(&open(&capture)?),
        Command::Draws {
            capture,
            vs,
            ps,
            cs,
            shader,
            pass,
            uses,
            target,
            content,
            call,
        } => draws::draws(
            &open(&capture)?,
            &draws::Filter {
                vs,
                ps,
                cs,
                shader,
                pass,
                uses,
                target,
                content,
                call,
            },
        ),
        Command::Draw { capture, n, all } => draws::draw(&open(&capture)?, n, all),
        Command::Cb {
            capture,
            draw,
            stage,
            slot,
            raw,
        } => draws::cb(&open(&capture)?, draw, &stage, slot, raw),
        Command::Find {
            capture,
            value,
            tolerance,
            int,
        } => draws::find(&open(&capture)?, value, tolerance, int),
        Command::Passes { capture } => draws::passes(&open(&capture)?),
        Command::Resources {
            capture,
            used,
            written,
            kind,
            format,
            width,
            height,
            hash,
        } => resources::resources(
            &open(&capture)?,
            &resources::Filter {
                used,
                written,
                kind,
                format,
                hash,
                width,
                height,
            },
        ),
        Command::Resource { capture, id } => resources::resource(&open(&capture)?, id),
        Command::Events {
            capture,
            kind,
            resource,
            from,
            limit,
        } => resources::events(
            &open(&capture)?,
            &resources::EventFilter {
                kind,
                resource,
                from,
                limit,
            },
        ),
        Command::Stack { capture, id } => resources::stack(&open(&capture)?, id),
        Command::Shaders { capture } => shaders::shaders(&open(&capture)?),
        Command::Shader {
            capture,
            hash,
            disassemble,
        } => shaders::shader(&open(&capture)?, &hash, disassemble),
        Command::Export {
            capture,
            id,
            out,
            sub,
            mip,
            slice,
            z,
            upload,
            range: r,
        } => export::export(
            &open(&capture)?,
            id,
            &out,
            &export::Which {
                sub,
                mip,
                slice,
                z,
                upload,
            },
            range(&r)?,
        ),
        Command::Target {
            capture,
            draw,
            name,
            out,
            range: r,
        } => export::target(&open(&capture)?, draw, &name, &out, range(&r)?),
        Command::Blob { capture, key, out } => export::blob(&open(&capture)?, &key, &out),
        Command::Sql { capture, query } => sql(&open(&capture)?, &query),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("locum-cli: {e}");
            ExitCode::FAILURE
        }
    }
}
