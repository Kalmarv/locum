//! locum-cli: reads the captures locum writes. A capture is named by its `capture.db`, the folder holding it, its
//! name under locum's folder (`LOCUM_DIR`, or `%LOCALAPPDATA%\locum`), or `last`.

mod describe;
mod diff;
mod draw;
mod draws;
mod dxil;
mod export;
mod info;
mod join;
mod memory;
mod open;
mod pixels;
mod replay;
mod resources;
mod shaders;
mod states;
mod trace;
mod upgrade;

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
    /// An index of files (`locum-cli index`) naming what resources were made from; repeat for several (default:
    /// LOCUM_INDEX, paths separated by ';').
    #[arg(long, global = true)]
    index: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// How a .png is drawn.
#[derive(clap::Args)]
struct PictureArgs {
    /// PNG values: auto (a single channel stretched from its least to its greatest value, float colour
    /// tone-mapped, the rest clamped), clamp, tonemap or LO:HI.
    #[arg(long, default_value = "auto")]
    range: String,
    /// PNG: one channel as grey (r, g, b or a); the mapping used is printed.
    #[arg(long, value_parser = ["r", "g", "b", "a"])]
    channel: Option<String>,
    /// PNG: only the texels from X,Y, W wide and H high.
    #[arg(long, value_name = "X,Y,W,H", conflicts_with = "crop")]
    rect: Option<String>,
    /// PNG: only the smallest rectangle holding every texel that is not zero (a sparse atlas's used part).
    #[arg(long)]
    crop: bool,
    /// PNG: the alpha channel as the picture's alpha. Pictures are opaque otherwise: a target's alpha is rarely
    /// coverage, and a zero there hides the colour (`--channel a` shows it as grey).
    #[arg(long)]
    alpha: bool,
}

impl PictureArgs {
    fn picture(&self) -> Result<export::Picture, String> {
        let rect = self
            .rect
            .as_ref()
            .map(|r| {
                let n: Vec<u32> = r.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                <[u32; 4]>::try_from(n).map_err(|_| format!("--rect {r}: not X,Y,W,H"))
            })
            .transpose()?;
        Ok(export::Picture {
            range: range(&self.range)?,
            channel: self.channel.as_deref().map(|c| "rgba".find(c).unwrap_or(0)),
            rect,
            crop: self.crop,
            alpha: self.alpha,
        })
    }
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
        /// Draws whose program at any stage has this debug name, or a part of it (any case): `--name SsaoBlur`.
        #[arg(long)]
        name: Option<String>,
        /// Draws whose programs declare a resource or constant buffer by this name (any case): `--binds g_LightBuffer`.
        #[arg(long)]
        binds: Option<String>,
    },
    /// One draw: its shaders, state, everything bound, its constants by name, and the targets read after it.
    Draw {
        capture: String,
        /// The draw's number, `last`, or `all` for every draw.
        n: String,
        /// Every bound register and every constant register, not only those the shaders use.
        #[arg(long)]
        all: bool,
        /// As JSON: one object per draw, one per line.
        #[arg(long)]
        json: bool,
        /// Only these parts after the call and shaders: state, bound, cb, targets (`--section bound,cb`).
        #[arg(long, value_delimiter = ',', value_parser = clap::builder::PossibleValuesParser::new(draw::SECTIONS))]
        section: Vec<String>,
    },
    /// A draw's constant buffer, every register, with names from the shader.
    Cb {
        capture: String,
        draw: i64,
        /// vs, hs, ds, gs, ps or cs.
        stage: String,
        slot: u32,
        /// Register space (D3D12); zero for D3D11.
        #[arg(long, default_value_t = 0)]
        space: u32,
        /// Hex bytes instead.
        #[arg(long, conflicts_with = "json")]
        raw: bool,
        /// As JSON: each 4-byte word with its register, name and value.
        #[arg(long)]
        json: bool,
        /// Only these registers: `--reg 3,26`.
        #[arg(long, value_delimiter = ',')]
        reg: Vec<u32>,
        /// Only registers holding a variable whose name has this in it (any case): `--name SC_CameraPos`.
        #[arg(long)]
        name: Option<String>,
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
    /// What output lanes of a draw are made of: the texture channels, constants (by name, with the draw's values) and
    /// vertex attributes (through the vertex program, by semantic, with the input-layout element) each comes from, and
    /// how: as its value, only as an address (a texel coordinate, an index), or only through a condition.
    Trace {
        capture: String,
        /// A draw number, or `last`.
        draw: String,
        /// SV_Target1.z, rt1 (every lane of target 1), o1.z (the DXBC register's lane), depth, TEXCOORD3.xy,
        /// SV_Position.z (the vertex program's) or discard; none: every lane the pixel program writes.
        lanes: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// A shader's constant buffers and, with --disassemble, its code; with --trace, what its output lanes are made of.
    Shader {
        capture: String,
        hash: String,
        /// What these output lanes are made of, to the program's own inputs (as `trace` takes them; none: every lane
        /// it writes). `trace` at a draw follows a pixel program's inputs into its vertex program.
        #[arg(long, num_args = 0.., value_name = "LANE", conflicts_with_all = ["disassemble", "wgsl", "evidence"])]
        trace: Option<Vec<String>>,
        #[arg(long, short, conflicts_with = "json")]
        disassemble: bool,
        /// Readable WGSL through iridia (original bytecode remains authoritative).
        #[arg(long, conflicts_with = "json")]
        wgsl: bool,
        /// The reflection as JSON: every binding, and each constant buffer's variables and every 4-byte value by
        /// its full name.
        #[arg(long)]
        json: bool,
        /// For DXIL, write dxil-reader's, Microsoft's and iridia's reflection and how they compare to this file.
        #[arg(long, conflicts_with = "json")]
        evidence: Option<PathBuf>,
    },
    /// A shader file (.dxbc, .cso): a DXIL program's reflection by dx, Microsoft's compiler and iridia side by side,
    /// or a DXBC program's constant buffers and bindings; with --disassemble its code, with --wgsl iridia's WGSL.
    ShaderFile {
        file: PathBuf,
        #[arg(long, short)]
        disassemble: bool,
        /// Readable WGSL through iridia (original bytecode remains authoritative).
        #[arg(long)]
        wgsl: bool,
        /// Write a DXIL program's independent answers, source path, hash and revisions as JSON.
        #[arg(long)]
        evidence: Option<PathBuf>,
    },
    /// A resource's contents as the frame began, or with --at as an event saw them: .dds (whole, or one
    /// subresource), .png (one subresource), or raw.
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
        #[arg(long, conflicts_with = "at")]
        upload: bool,
        /// Its contents just before this event (a draw's event: what the draw saw), replayed from the frame's start
        /// with the events that wrote it.
        #[arg(long)]
        at: Option<i64>,
        /// Replay on WARP (with --at).
        #[arg(long, requires = "at")]
        warp: bool,
        #[command(flatten)]
        picture: PictureArgs,
    },
    /// A target read back after a pass's last draw, as .png, .dds or raw (`-o -`: the raw bytes to stdout).
    Target {
        capture: String,
        /// The draw the target was read after, or `last` for the frame's last readback of it (the presented picture,
        /// for the back buffer's rt0).
        draw: String,
        /// rt0 to rt7, or depth.
        #[arg(default_value = "rt0")]
        name: String,
        #[arg(short, long)]
        out: PathBuf,
        #[command(flatten)]
        picture: PictureArgs,
    },
    /// One texel of a target in every readback of it after a pass, in the frame's order: where each pass left it,
    /// which changed it, and the pass ends not read back.
    Pixel {
        capture: String,
        /// The target: `#ID`, or `DRAW:TARGET` (`1935:rt0`) for the resource and subresource a readback holds.
        target: String,
        x: u32,
        y: u32,
        /// The subresource (default: the one DRAW:TARGET names, or the resource's first read back).
        #[arg(long)]
        sub: Option<u32>,
        /// Only the readbacks where the texel changed.
        #[arg(long)]
        changed: bool,
    },
    /// For every readback of a target after a pass, how many texels differ from the readback before and where.
    Changes {
        capture: String,
        /// As for `pixel`.
        target: String,
        #[arg(long)]
        sub: Option<u32>,
        /// Only the texels from X0,Y0 up to (not including) X1,Y1.
        #[arg(long, value_name = "X0,Y0,X1,Y1")]
        rect: Option<String>,
    },
    /// A blob from the store, by key (`-o -`: to stdout).
    Blob {
        capture: String,
        key: String,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Replays a draw offline (with what it depends on), optionally with rules changing it, and writes what it drew.
    /// With rules, writes the draw's targets as recorded (`.recorded`) and with the rules (`.rules`), and where they
    /// differ (`.diff.png`).
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
        /// Enable D3D11 validation; errors make verification fail.
        #[arg(long)]
        debug: bool,
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
        /// Enable D3D11 validation; errors make verification fail.
        #[arg(long)]
        debug: bool,
        /// Compare at most this many targets.
        #[arg(long)]
        limit: Option<i64>,
        /// Write what differs here: the replay's and the game's bytes of each output and target, and pictures of
        /// the targets and where they differ.
        #[arg(short, long)]
        out: Option<PathBuf>,
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
        /// Wait for this event or target exit before taking a crash snapshot.
        #[arg(long)]
        wait_event: Option<String>,
        /// Signal after opening the target and crash-request event.
        #[arg(long, requires = "wait_event")]
        ready_event: Option<String>,
        /// Remote address of two native-width words: fault thread ID and EXCEPTION_POINTERS.
        #[arg(long, requires = "wait_event")]
        exception_info: Option<usize>,
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
    /// Adds DDS files (each file given, and every one under each folder given) to an index of files by the keys of
    /// their bytes, made if there is none.
    Index {
        /// The index to make or add to.
        #[arg(short, long)]
        out: PathBuf,
        inputs: Vec<PathBuf>,
        /// A list of buffer keys to add, one `key<TAB>bytes<TAB>name` a line, as a game's reader writes them for
        /// what is inside its archives (fromsoft-formats' `flver_keys` example: FLVER vertex and index bytes).
        #[arg(long)]
        keys: Vec<PathBuf>,
    },
    /// What each resource the frame used was made from, by the indexes given (--index).
    Join {
        capture: String,
        /// Every resource alive at the capture, not only those the frame used.
        #[arg(long)]
        all: bool,
        /// List those not found instead.
        #[arg(long)]
        unnamed: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Any SQL on the capture; rows tab-separated.
    Sql { capture: String, query: String },
    /// Brings what an older locum derived in a capture up to this one, from the capture's own records: D3D12
    /// bindings projected again (an older projection bound some registers to two descriptors), structured views'
    /// element size, and the programs' debug names in `shaders`. Writes the capture.db in place
    Upgrade {
        capture: String,
        /// Say what it would change, and write nothing
        #[arg(long)]
        check: bool,
    },
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
    let indexes = join::paths(&cli.index);
    let open = |name: &str| open::open(name, blobs)?.named(&indexes);
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
            name,
            binds,
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
                name,
                binds,
            },
        ),
        Command::Draw {
            capture,
            n,
            all,
            json,
            section,
        } => draw::draw(&open(&capture)?, &n, all, json, &section),
        Command::Cb {
            capture,
            draw,
            stage,
            slot,
            space,
            raw,
            json,
            reg,
            name,
        } => draw::cb(
            &open(&capture)?,
            draw,
            &stage,
            slot,
            space,
            raw,
            json,
            &draw::Only { registers: reg, name },
        ),
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
            trace: Some(lanes),
            json,
            ..
        } => trace::program(&open(&capture)?, &hash, &lanes, json),
        Command::Trace {
            capture,
            draw,
            lanes,
            json,
        } => trace::draw(&open(&capture)?, &draw, &lanes, json),
        Command::Shader {
            capture,
            hash,
            disassemble,
            wgsl,
            json,
            evidence,
            trace: None,
        } => shaders::shader(
            &open(&capture)?,
            &hash,
            disassemble,
            wgsl,
            json,
            evidence.as_deref(),
        ),
        Command::ShaderFile {
            file,
            disassemble,
            wgsl,
            evidence,
        } => shaders::file(&file, disassemble, wgsl, evidence.as_deref()),
        Command::Export {
            capture,
            id,
            out,
            sub,
            mip,
            slice,
            z,
            upload,
            at,
            warp,
            picture,
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
                at,
                warp,
            },
            &picture.picture()?,
        ),
        Command::Target {
            capture,
            draw,
            name,
            out,
            picture,
        } => {
            let c = open(&capture)?;
            let draw = match draw.as_str() {
                "last" => c
                    .row(
                        "SELECT draw FROM targets WHERE target = ?1 AND blob IS NOT NULL ORDER BY event DESC LIMIT 1",
                        [&name],
                        |r| r.get(0),
                    )
                    .ok_or_else(|| format!("no {name} was read back"))?,
                n => n.parse().map_err(|_| format!("{n}: not a draw number or `last`"))?,
            };
            export::target(&c, draw, &name, &out, &picture.picture()?)
        }
        Command::Pixel {
            capture,
            target,
            x,
            y,
            sub,
            changed,
        } => pixels::pixel(&open(&capture)?, &target, x, y, sub, changed),
        Command::Changes {
            capture,
            target,
            sub,
            rect,
        } => {
            let rect = rect
                .map(|r| {
                    let n: Vec<u32> = r.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                    <[u32; 4]>::try_from(n).map_err(|_| format!("--rect {r}: not X0,Y0,X1,Y1"))
                })
                .transpose()?;
            pixels::changes(&open(&capture)?, &target, sub, rect)
        }
        Command::Blob { capture, key, out } => export::blob(&open(&capture)?, &key, &out),
        Command::Replay {
            capture,
            draw,
            rule,
            rules,
            mode,
            warp,
            debug,
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
                    debug,
                    out,
                    range: range(&r)?,
                },
            )
        }
        Command::Verify {
            capture,
            warp,
            debug,
            limit,
            out,
        } => replay::verify(&open(&capture)?, warp, debug, limit, out.as_deref()),
        Command::Dump {
            pid,
            out,
            signal,
            wait_event,
            ready_event,
            exception_info,
        } => memory::dump(
            pid,
            &out,
            signal.as_deref(),
            wait_event.as_deref(),
            ready_event.as_deref(),
            exception_info,
        ),
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
        Command::Index { out, inputs, keys } => join::index(&out, &inputs, &keys),
        Command::Join {
            capture,
            all,
            unnamed,
            limit,
        } => join::join(&open(&capture)?, all, unnamed, limit),
        Command::Sql { capture, query } => sql(&open(&capture)?, &query),
        Command::Upgrade { capture, check } => upgrade::upgrade(&open(&capture)?, check),
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
