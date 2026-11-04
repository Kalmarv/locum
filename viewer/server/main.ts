// The locum viewer's server: a JSON API over captures (capture.db read-only, plus the blob store) and the built
// front end. Pictures are decoded by locum-cli and cached under viewer/.cache.
//
//   bun run viewer [--db PATH] [--blobs DIR] [--dir DIR] [--port N] [--host H] [--cli PATH]
//   bun run dev    (the same, with vite serving the front end with hot reload)

import { Database } from "bun:sqlite";
import { parseArgs } from "node:util";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { Capture, listCaptures, summarize, type Source } from "./capture";
import { Cli, cacheRoot, findCli } from "./cli";
import { CHANNELS, decodePng, encodePng, selectChannels, type Channels } from "./png";
import { formatNames } from "./format";

const { values: args } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    db: { type: "string" },
    blobs: { type: "string" },
    dir: { type: "string" },
    port: { type: "string" },
    host: { type: "string" },
    cli: { type: "string" },
    dev: { type: "boolean" },
    help: { type: "boolean", short: "h" },
  },
});

if (args.help) {
  console.log(`locum viewer: browse a locum capture in the browser

  bun run viewer [--db PATH] [options]

  --db PATH     a capture.db (or a folder holding one) to open at start
  --blobs DIR   the blob store (default: LOCUM_BLOBS, else the one the capture names)
  --dir DIR     the folder listed for captures (default: the --db's captures folder, LOCUM_DIR, or %LOCALAPPDATA%/locum)
  --port N      default 5170 (the next free one if taken)
  --host H      default 127.0.0.1; the API reads any capture.db the server can see, so keep it local
  --cli PATH    locum-cli (default: LOCUM_CLI, PATH, ../target/release)`);
  process.exit(0);
}

const viewerRoot = resolve(import.meta.dir, "..");
const dist = join(viewerRoot, "dist");
const cli = new Cli(findCli(args.cli));
const registryPath = join(cacheRoot, "captures.json");

interface Entry {
  path: string;
  blobs: string | null;
  label: string;
  uploaded?: boolean;
}

mkdirSync(cacheRoot, { recursive: true });
let registry: Record<string, Entry> = {};
try {
  registry = JSON.parse(readFileSync(registryPath, "utf8"));
} catch {}
const open = new Map<string, Capture>();

function saveRegistry() {
  writeFileSync(registryPath, JSON.stringify(registry, null, 1));
}

/** The capture.db a path names: the file, or the capture.db in the folder. */
function dbPath(given: string): string {
  const p = resolve(given);
  if (existsSync(p) && statSync(p).isFile()) return p;
  if (existsSync(join(p, "capture.db"))) return join(p, "capture.db");
  throw new HttpError(404, `${given} is not a capture.db or a folder holding one`);
}

/** The blob store for a capture: given, LOCUM_BLOBS, the one it names, or `blobs` beside its captures folder. */
function blobRoot(path: string, given: string | null): string {
  if (given) return resolve(given);
  if (args.blobs) return resolve(args.blobs);
  if (process.env.LOCUM_BLOBS) return resolve(process.env.LOCUM_BLOBS);
  let named: string | null = null;
  try {
    const db = new Database(path, { readonly: true });
    named = (db.query("SELECT value FROM meta WHERE key = 'blobs'").get() as any)?.value ?? null;
    db.close();
  } catch {}
  if (named && existsSync(named)) return resolve(named);
  const beside = resolve(dirname(path), "../../blobs");
  if (existsSync(beside)) return beside;
  return named ? resolve(named) : beside;
}

function idFor(text: string): string {
  return Bun.hash(text.toLowerCase()).toString(16).padStart(16, "0").slice(0, 12);
}

function openPath(given: string, blobs: string | null): Capture {
  const path = dbPath(given);
  const id = idFor(path);
  const existing = open.get(id);
  const root = blobRoot(path, blobs);
  if (existing && existing.blobs === root) return existing;
  existing?.close();
  return register(id, { path, blobs: blobs ? resolve(blobs) : null, label: labelOf(path) });
}

function labelOf(path: string): string {
  const s = summarize(path) as any;
  return s.name ?? path.split(/[\\/]/).slice(-2, -1)[0] ?? "capture";
}

function register(id: string, entry: Entry): Capture {
  const c = new Capture(id, entry.path, blobRoot(entry.path, entry.blobs), entry.label);
  open.set(id, c);
  registry[id] = entry;
  saveRegistry();
  return c;
}

function capture(id: string): Capture {
  let c = open.get(id);
  if (c) return c;
  const entry = registry[id];
  if (!entry || !existsSync(entry.path)) throw new HttpError(404, `capture ${id} is not open (open it again)`);
  c = new Capture(id, entry.path, blobRoot(entry.path, entry.blobs), entry.label);
  open.set(id, c);
  return c;
}

/** Stores a capture.db sent by the browser (drag and drop, or the File System Access API) and opens it. */
async function upload(req: Request, name: string, blobs: string | null): Promise<Capture> {
  if (!req.body) throw new HttpError(400, "no file");
  const dir = join(cacheRoot, "uploads");
  mkdirSync(dir, { recursive: true });
  const tmp = join(dir, `.upload-${process.pid}-${Date.now()}.part`);
  const hasher = new Bun.CryptoHasher("sha256");
  const sink = Bun.file(tmp).writer();
  let size = 0;
  try {
    for await (const chunk of req.body as any as AsyncIterable<Uint8Array>) {
      hasher.update(chunk);
      sink.write(chunk);
      size += chunk.length;
    }
    await sink.end();
  } catch (e) {
    rmSync(tmp, { force: true });
    throw e;
  }
  const hash = hasher.digest("hex").slice(0, 16);
  const folder = join(dir, hash);
  const path = join(folder, "capture.db");
  mkdirSync(folder, { recursive: true });
  if (existsSync(path)) rmSync(tmp, { force: true });
  else renameSync(tmp, path);
  const header = new Uint8Array(await Bun.file(path).slice(0, 16).arrayBuffer());
  if (new TextDecoder().decode(header) !== "SQLite format 3\0") throw new HttpError(400, `${name} is not an SQLite file`);
  const id = `u${hash.slice(0, 11)}`;
  open.get(id)?.close();
  open.delete(id);
  const label = (summarize(path) as any).name ?? name.replace(/\.db$/, "");
  console.log(`opened ${name} from the browser (${(size / 1048576).toFixed(1)} MiB) as ${id}`);
  return register(id, { path, blobs, label, uploaded: true });
}

class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

function jsonResponse(body: unknown, req: Request, status = 200): Response {
  const text = JSON.stringify(body);
  const headers: Record<string, string> = { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" };
  if (text.length > 64 * 1024 && (req.headers.get("accept-encoding") ?? "").includes("gzip")) {
    headers["content-encoding"] = "gzip";
    return new Response(Bun.gzipSync(text, { level: 4 }), { status, headers });
  }
  return new Response(text, { status, headers });
}

/** `target:DRAW:NAME`, `resource:ID:SUB[:Z]` or `upload:ID:SUB[:Z]`. */
function parseSource(text: string | null): Source {
  const parts = (text ?? "").split(":");
  const n = (i: number) => {
    const v = Number(parts[i] ?? 0);
    if (!Number.isInteger(v) || v < 0) throw new HttpError(400, `bad source ${text}`);
    return v;
  };
  if (parts[0] === "target" && /^(rt[0-7]|depth)$/.test(parts[2] ?? "")) return { kind: "target", draw: n(1), name: parts[2]! };
  if (parts[0] === "resource" || parts[0] === "upload") return { kind: parts[0], id: n(1), sub: n(2), z: n(3) };
  throw new HttpError(400, `bad source ${text}`);
}

/** Where outputs of locum-cli for a capture are kept: a new capture file or a rebuilt locum-cli starts afresh. */
function cacheDir(c: Capture): string {
  const st = statSync(c.path);
  const exe = cli.exe && existsSync(cli.exe) ? statSync(cli.exe) : null;
  const tool = exe ? `${Math.round(exe.mtimeMs).toString(36)}${exe.size.toString(36)}` : "none";
  return join(cacheRoot, "img", `${c.id}-${Math.round(st.mtimeMs)}-${st.size}-${tool}`);
}

const RANGE = /^(auto|clamp|tonemap|-?[0-9.eE+-]+:-?[0-9.eE+-]+)$/;

/** A PNG of a source, made by locum-cli, then reduced to `channels` (see selectChannels). */
async function image(c: Capture, src: Source, range: string, channels: string): Promise<string> {
  if (!RANGE.test(range)) throw new HttpError(400, `bad range ${range}`);
  if (!(CHANNELS as readonly string[]).includes(channels)) throw new HttpError(400, `bad channels ${channels}`);
  const tag = range.replace(/[^a-z0-9.+-]/gi, "_");
  // With alpha: the page shows the channels, or the colour without it, itself.
  const common = ["--blobs", c.blobs, "--range", range, "--alpha"];
  let full: string;
  if (src.kind === "target") {
    const out = join(cacheDir(c), `target-${src.draw}-${src.name}-${tag}.png`);
    full = await cli.cached(out, tmp => cli.run(["target", c.path, String(src.draw), src.name, "-o", tmp, ...common]).then(() => {}));
  } else {
    const out = join(cacheDir(c), `${src.kind}-${src.id}-${src.sub}-${src.z ?? 0}-${tag}.png`);
    full = await cli.cached(out, tmp => cli.run(["export", c.path, String(src.id), "-o", tmp, "--sub", String(src.sub),
      "--z", String(src.z ?? 0), ...(src.kind === "upload" ? ["--upload"] : []), ...common]).then(() => {}));
  }
  if (channels === "rgba") return full;
  return cli.cached(full.replace(/\.png$/, `-${channels}.png`), async tmp => {
    const p = decodePng(new Uint8Array(await Bun.file(full).arrayBuffer()));
    await Bun.write(tmp, encodePng(p.width, p.height, selectChannels(p.rgba, channels as Channels)));
  });
}

const HEX = /^[0-9a-f]{8,64}$/i;

async function api(req: Request, url: URL): Promise<Response> {
  const path = url.pathname.slice("/api/".length).split("/").map(decodeURIComponent);
  const q = url.searchParams;
  const json = (body: unknown) => jsonResponse(body, req);

  if (path[0] === "status") {
    return json({
      cli: cli.exe,
      defaultDir: defaultDir(),
      initial: initial?.id ?? null,
      recent: Object.entries(registry).map(([id, e]) => ({ id, ...e })).filter(e => existsSync(e.path)),
      formats: formatNames(),
    });
  }
  if (path[0] === "captures") {
    const dir = resolve(q.get("dir") || defaultDir());
    const list = await listCaptures(dir);
    return json({ dir, captures: list.map(l => ({ ...l, ...summarize(l.path) })) });
  }
  if (path[0] === "open" && req.method === "POST") {
    const body = (await req.json()) as { path?: string; blobs?: string };
    if (!body.path) throw new HttpError(400, "no path");
    return json(openPath(body.path, body.blobs || null).info());
  }
  if (path[0] === "upload" && req.method === "POST") {
    const c = await upload(req, q.get("name") ?? "capture.db", q.get("blobs") || null);
    return json(c.info());
  }
  if (path[0] !== "c" || !path[1]) throw new HttpError(404, "no such API");

  const c = capture(path[1]);
  const what = path[2];
  const n = (i: number) => {
    const v = Number(path[i]);
    if (!Number.isInteger(v)) throw new HttpError(400, `bad number ${path[i]}`);
    return v;
  };
  const found = <T>(v: T | null, what: string): T => {
    if (v == null) throw new HttpError(404, `no ${what}`);
    return v;
  };

  switch (what) {
    case "info":
      return json(c.info());
    case "timeline":
      return json(c.timeline());
    case "event":
      return json(found(c.event(n(3)), `event ${path[3]}`));
    case "draw":
      return json(found(c.draw(n(3)), `draw ${path[3]}`));
    case "resources":
      return json(c.resources());
    case "resource":
      return json(found(c.resource(n(3)), `resource ${path[3]}`));
    case "shaders":
      return json(c.shaders());
    case "shader": {
      const hash = path[3] ?? "";
      if (!HEX.test(hash)) throw new HttpError(400, "bad hash");
      if (path[4] === "disasm") {
        const out = join(cacheDir(c), `shader-${hash}.txt`);
        return new Response(await cli.cachedText(out, ["shader", c.path, hash, "-d", "--blobs", c.blobs]),
          { headers: { "content-type": "text/plain; charset=utf-8" } });
      }
      return json(found(c.shader(hash), `shader ${hash}`));
    }
    case "stack":
      return json(c.stack(n(3)));
    case "blob": {
      const key = path[3] ?? "";
      if (!HEX.test(key)) throw new HttpError(400, "bad key");
      if (!c.hasBlob(key)) throw new HttpError(404, `blob ${key} is not in ${c.blobs}`);
      const offset = Number(q.get("offset") ?? 0), size = Math.min(Number(q.get("size") ?? 65536), 16 << 20);
      return new Response(c.readBlob(key, offset, size), { headers: { "content-type": "application/octet-stream",
        "x-blob-size": String(c.blobSize(key)) } });
    }
    case "image": {
      const file = await image(c, parseSource(q.get("src")), q.get("range") || "auto", q.get("channels") || "rgb");
      return new Response(Bun.file(file), { headers: { "content-type": "image/png", "cache-control": "max-age=3600" } });
    }
    case "texel": {
      const x = Number(q.get("x")), y = Number(q.get("y"));
      return json(c.texel(parseSource(q.get("src")), x, y));
    }
    case "diff": {
      const d = c.diff(parseSource(q.get("a")), parseSource(q.get("b")));
      if (typeof d === "string") throw new HttpError(422, d);
      return new Response(encodePng(d.width, d.height, d.rgba), { headers: { "content-type": "image/png",
        "x-changed": String(d.changed), "x-total": String(d.total), "access-control-expose-headers": "x-changed, x-total" } });
    }
    case "sql": {
      const body = (await req.json()) as { sql?: string; limit?: number };
      try {
        return json(c.sql(body.sql ?? "", Math.min(body.limit ?? 2000, 100000)));
      } catch (e) {
        throw new HttpError(400, String((e as Error).message ?? e));
      }
    }
    case "verify": {
      const out = join(cacheDir(c), "verify.txt");
      if (req.method !== "POST") {
        return json({ text: existsSync(out) ? readFileSync(out, "utf8") : null });
      }
      if (q.get("again")) rmSync(out, { force: true });
      // Verify's report matters most when targets differ, which it reports with a failing exit code.
      await cli.cached(out, async tmp => {
        const r = await cli.runAll(["verify", c.path, "--blobs", c.blobs], 30 * 60_000);
        await Bun.write(tmp, `${r.stdout}${r.stderr ? `
${r.stderr}` : ""}
(locum-cli verify exited with ${r.code})
`);
      });
      return json({ text: readFileSync(out, "utf8") });
    }
  }
  throw new HttpError(404, "no such API");
}

function defaultDir(): string {
  if (args.dir) return resolve(args.dir);
  if (args.db) return resolve(dirname(dbPath(args.db)), "..");
  if (process.env.LOCUM_DIR) return resolve(process.env.LOCUM_DIR);
  return join(process.env.LOCALAPPDATA ?? join(process.env.HOME ?? ".", ".local/share"), "locum");
}

async function serveStatic(url: URL): Promise<Response> {
  if (!existsSync(join(dist, "index.html"))) {
    return new Response("The front end is not built: run `bun run viewer` (it builds first) or `bun run dev`.", { status: 503 });
  }
  const rel = decodeURIComponent(url.pathname).replace(/^\/+/, "");
  const file = resolve(dist, rel || "index.html");
  if (file.startsWith(dist) && existsSync(file) && statSync(file).isFile()) {
    return new Response(Bun.file(file), {
      headers: rel.startsWith("assets/") ? { "cache-control": "max-age=31536000, immutable" } : {},
    });
  }
  return new Response(Bun.file(join(dist, "index.html")));
}

let initial: Capture | null = null;
if (args.db) {
  initial = openPath(args.db, args.blobs ?? null);
  const info = initial.info();
  console.log(`capture ${info.label}: ${info.timeline.events} events, ${info.timeline.draws} draws, blobs ${info.blobs}`);
  for (const m of info.missing) console.log(`  missing: ${m}`);
}
if (!cli.exe) console.warn("locum-cli not found: pictures and disassembly are off (pass --cli or set LOCUM_CLI)");

const host = args.host ?? "127.0.0.1";
const wanted = Number(args.port ?? (args.dev ? 5171 : 5170));

function serve(port: number) {
  return Bun.serve({
    hostname: host,
    port,
    idleTimeout: 255,
    async fetch(req) {
      const url = new URL(req.url);
      if (!url.pathname.startsWith("/api/")) return serveStatic(url);
      try {
        return await api(req, url);
      } catch (e) {
        const status = e instanceof HttpError ? e.status : 500;
        if (status === 500) console.error(e);
        return jsonResponse({ error: String((e as Error).message ?? e) }, req, status);
      }
    },
  });
}

// `bun run viewer` starts this server as its child, and on Windows ending `bun run` (a task runner stopping it, a
// closed terminal) does not end the child: the server ends with the process that started it.
const parent = process.ppid;
if (parent > 1) {
  setInterval(() => {
    try {
      process.kill(parent, 0);
    } catch {
      process.exit(0);
    }
  }, 2000);
}

let server: ReturnType<typeof serve> | null = null;
for (let port = wanted; port < wanted + 20 && !server; port++) {
  try {
    server = serve(port);
  } catch (e) {
    if (!String(e).includes("EADDRINUSE") && !String(e).toLowerCase().includes("in use")) throw e;
  }
}
if (!server) throw new Error(`no free port from ${wanted}`);
const apiUrl = `http://${host === "0.0.0.0" ? "localhost" : host}:${server.port}`;
const hash = initial ? `#/c/${initial.id}` : "";

if (args.dev) {
  const vite = Bun.spawn([process.execPath, "x", "vite", "--port", "5173", "--strictPort", "false"], {
    cwd: viewerRoot, env: { ...process.env, LOCUM_VIEWER_API: apiUrl }, stdout: "inherit", stderr: "inherit",
  });
  process.on("SIGINT", () => { vite.kill(); process.exit(0); });
  console.log(`API on ${apiUrl}; vite prints the page's URL (open it with ${hash || "#/"})`);
} else {
  console.log(`locum viewer: ${apiUrl}/${hash}`);
}
