//! locum-cli: reads the captures locum writes. A capture is named by its `capture.db`, the folder holding it, its
//! name under locum's folder (`LOCUM_DIR`, or `%LOCALAPPDATA%\locum`), or `last`.

mod describe;
mod diff;
mod draws;
mod export;
mod info;
mod memory;
mod open;
mod replay;
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
    /// Replays a draw offline (with what it depends on), optionally with rules changing it, and writes what it drew.
    /// With rules, writes the draw's targets before and after them and where they differ.
    Replay {
        capture: String,
        /// The draw's number, or `last`.
        draw: String,
        /// A rule as in overrides.txt (`constant ps b3 c0 1 0 0 1`, `skip vs=<hash>`, ...); may be given many times.
        #[arg(long)]
        rule: Vec<String>,
        /// A file of rules.
        #[arg(long)]
        rules: Option<PathBuf>,
        /// slice (the draw and what it depends on), frame (everything up to it), or draw (it alone).
        #[arg(long, default_value = "slice")]
        mode: String,
        /// Run on WARP, the software rasterizer, instead of the GPU.
        #[arg(long)]
        warp: bool,
        #[arg(short, long, default_value = "replay")]
        out: PathBuf,
        #[arg(long, default_value = "auto")]
        range: String,
    },
    /// Replays the whole frame and compares every target the capture read back.
    Verify {
        capture: String,
        #[arg(long)]
        warp: bool,
        /// Compare at most this many targets.
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Writes a full-memory minidump of a process from a snapshot of it (locum runs this at a capture: settings.txt
    /// `dumper`).
    Dump {
        #[arg(long)]
        pid: u32,
        #[arg(long)]
        out: PathBuf,
        /// An event to set once the snapshot is taken.
        #[arg(long)]
        signal: Option<String>,
    },
    /// Searches a capture's memory dump (or a .dmp file) for a value: a float, an integer, bytes, or the registers of
    /// a constant buffer a draw had bound.
    Memfind {
        /// A capture, or a .dmp file.
        target: String,
        #[arg(long, allow_negative_numbers = true)]
        float: Option<f32>,
        /// Relative tolerance for --float.
        #[arg(long, default_value_t = 1e-6)]
        tolerance: f32,
        #[arg(long, allow_negative_numbers = true)]
        int: Option<i64>,
        /// Hex bytes, like 0000803f.
        #[arg(long)]
        bytes: Option<String>,
        /// DRAW STAGE SLOT: each non-zero register of that constant buffer (or --register N alone).
        #[arg(long, num_args = 3, value_names = ["DRAW", "STAGE", "SLOT"])]
        cb: Option<Vec<String>>,
        #[arg(long)]
        register: Option<u32>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Compares two memory dumps at the same addresses: words that went from one float to another, or with neither
    /// given, how many words changed in each region.
    Memdiff {
        a: String,
        b: String,
        #[arg(long, allow_negative_numbers = true)]
        from: Option<f32>,
        #[arg(long, allow_negative_numbers = true)]
        to: Option<f32>,
        #[arg(long, default_value_t = 1e-6)]
        tolerance: f32,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Compares two captures draw by draw (matched by shaders, inputs and order, not by numbers that change between
    /// launches): what differs, in how many draws, and the draws only one has.
    Diff {
        a: String,
        b: String,
        /// Every differing draw, not only the summary.
        #[arg(long)]
        draws: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
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
        Command::Replay {
            capture,
            draw,
            rule,
            rules,
            mode,
            warp,
            out,
            range: r,
        } => {
            let c = open(&capture)?;
            let n = replay::draw_number(&c, &draw)?;
            let mode =
                replay::Mode::parse(&mode).ok_or_else(|| format!("--mode {mode}: slice, frame or draw"))?;
            replay::replay(
                &c,
                n,
                &replay::Options {
                    mode,
                    rules: rule,
                    rules_file: rules,
                    warp,
                    out,
                    range: range(&r)?,
                },
            )
        }
        Command::Verify { capture, warp, limit } => replay::verify(&open(&capture)?, warp, limit),
        Command::Dump { pid, out, signal } => memory::dump(pid, &out, signal.as_deref()),
        Command::Memfind {
            target,
            float,
            tolerance,
            int,
            bytes,
            cb,
            register,
            limit,
        } => {
            let needle = match (float, int, bytes, cb) {
                (Some(f), None, None, None) => memory::Needle::Float(f, tolerance),
                (None, Some(i), None, None) => memory::Needle::Int(i),
                (None, None, Some(hex), None) => {
                    let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
                    let bytes = (0..hex.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("x"), 16))
                        .collect::<Result<Vec<u8>, _>>()
                        .map_err(|_| format!("--bytes {hex}: not hex bytes"))?;
                    memory::Needle::Bytes(bytes)
                }
                (None, None, None, Some(cb)) => memory::Needle::Cb(
                    cb[0]
                        .parse()
                        .map_err(|_| format!("{}: not a draw number", cb[0]))?,
                    cb[1].clone(),
                    cb[2].parse().map_err(|_| format!("{}: not a slot", cb[2]))?,
                    register,
                ),
                _ => return Err("give one of --float, --int, --bytes or --cb".into()),
            };
            let (dump, capture) = memory::open(&target, blobs)?;
            memory::memfind(&dump, capture.as_ref(), &needle, limit)
        }
        Command::Memdiff {
            a,
            b,
            from,
            to,
            tolerance,
            limit,
        } => {
            let (a, _) = memory::open(&a, blobs)?;
            let (b, _) = memory::open(&b, blobs)?;
            memory::memdiff(&a, &b, from, to, tolerance, limit)
        }
        Command::Diff { a, b, draws, limit } => diff::diff(&open(&a)?, &open(&b)?, draws, limit),
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
