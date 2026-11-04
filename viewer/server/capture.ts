// One capture: its capture.db opened read-only, its blob store, and the queries the viewer's API answers.
// The schema is capture/src/db.rs; what each column means is written there.

import { Database, type SQLQueryBindings } from "bun:sqlite";
import { existsSync, openSync, readSync, closeSync, statSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { constantBuffers, nameAt, type ConstantBuffer } from "./rdef";
import { formatName, readable, readTexel, textureSubs, type Sub, layout } from "./format";

type Row = Record<string, any>;

/** The schema versions this viewer reads, by API: capture/src/db.rs `SCHEMA_VERSION` and `D3D12_SCHEMA_VERSION`. A
 * schema bump that changes no table the viewer queries only raises the upper bound. */
export const SCHEMAS: Record<string, [number, number]> = { d3d11: [1, 1], d3d12: [2, 5] };

export function schemaReadable(api: string, version: string): boolean {
  const range = SCHEMAS[api];
  return !!range && /^\d+$/.test(version) && Number(version) >= range[0] && Number(version) <= range[1];
}

const STAGE_ORDER: Record<string, number> = { ia: 0, vs: 1, hs: 2, ds: 3, gs: 4, so: 5, ps: 6, cs: 7, om: 8 };
const KIND_ORDER: Record<string, number> = { ib: 0, vb: 1, cb: 2, srv: 3, uav: 4, sampler: 5, so: 6, rtv: 7, dsv: 8 };

export interface Shape {
  id: number;
  kind: string;
  width: number;
  height: number;
  depth: number;
  mips: number;
  array: number;
  format: number;
  samples: number;
  bytes: number;
  stride: number;
  misc: number;
}

/** A resource's shape in words, as `locum-cli` writes it: `#812 texture2d 512x512 BC7_UNORM 10 mips`. */
export function describeShape(s: Shape): string {
  if (s.kind === "buffer") return `#${s.id} buffer ${s.width} bytes${s.stride ? ` stride ${s.stride}` : ""}`;
  let t = `#${s.id} ${s.kind} ${s.width}`;
  if (s.kind !== "texture1d") t += `x${s.height}`;
  if (s.kind === "texture3d") t += `x${s.depth}`;
  t += ` ${formatName(s.format)}`;
  if (s.mips > 1) t += ` ${s.mips} mips`;
  if (s.misc & 4) t += ` cube${s.array > 6 ? ` x${s.array / 6}` : ""}`;
  else if (s.array > 1) t += ` [${s.array}]`;
  if (s.samples > 1) t += ` ${s.samples}x MSAA`;
  return t;
}

const SHAPE = "id, kind, width, height, depth, mips, array, format, samples, bytes, stride, misc";

function shapeOf(r: Row): Shape {
  const n = (v: any) => (v == null ? 0 : Number(v));
  return {
    id: r.id, kind: r.kind, width: n(r.width), height: n(r.height), depth: n(r.depth), mips: n(r.mips),
    array: n(r.array), format: n(r.format), samples: n(r.samples), bytes: n(r.bytes), stride: n(r.stride),
    misc: n(r.misc),
  };
}

function json(text: string | null | undefined): any {
  if (text == null) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** Where a picture's texels come from: a target read back after a pass, or a resource's contents as the frame began. */
export type Source =
  | { kind: "target"; draw: number; name: string }
  | { kind: "resource"; id: number; sub: number; z?: number }
  | { kind: "upload"; id: number; sub: number; z?: number };

/** The bytes behind a source: a blob, the range of it holding the subresource, and how to read its texels. */
export interface Texels {
  blob: string;
  offset: number;
  size: number;
  format: number;
  width: number;
  height: number;
  /** Bytes in one depth slice (volume textures keep slices one after another). */
  sliceBytes: number;
}

export interface TimelineRow {
  seq: number;
  kind: string;
  call: string;
  context: number;
  deferred: number;
  list: number | null;
  draw: number | null;
  pass: number | null;
  group: number;
  args: string;
  data: string | null;
  vs: string | null;
  hs: string | null;
  ds: string | null;
  gs: string | null;
  ps: string | null;
  cs: string | null;
}

export interface Group {
  /** The pass number, or null for a run of dispatches (or events before any draw). */
  pass: number | null;
  label: string;
  first: number;
  count: number;
}

/**
 * How long a capture.db stays open after its last query. On Windows an open handle stops whatever wrote the capture
 * from replacing it (a smoke run, a new capture of the same name), so the viewer holds it only while it reads.
 */
const IDLE_MS = 1500;

export class Capture {
  private handle: Database | null = null;
  private idle: ReturnType<typeof setTimeout> | null = null;
  /** The file's mtime and size when it was last opened: when they change, what was read from it is dropped. */
  private stamp = "";
  private reflection = new Map<string, ConstantBuffer[] | null>();
  private timelineCache: { rows: TimelineRow[]; groups: Group[] } | null = null;
  private modulesCache: { path: string; base: bigint; size: bigint }[] | null = null;
  private features: { targetEvent: boolean; results: boolean } | null = null;
  private blobIndex: Database | null = null;

  constructor(readonly id: string, readonly path: string, readonly blobs: string, readonly label: string) {}

  /** The database, opened on first use and closed again once idle. */
  private get db(): Database {
    if (!this.handle) {
      const st = statSync(this.path);
      const stamp = `${st.mtimeMs}-${st.size}`;
      if (stamp !== this.stamp) {
        this.stamp = stamp;
        this.reflection.clear();
        this.timelineCache = null;
        this.modulesCache = null;
        this.features = null;
      }
      this.handle = new Database(this.path, { readonly: true });
      this.handle.exec("PRAGMA query_only = ON");
      const meta = Object.fromEntries((this.handle.query("SELECT key,value FROM meta WHERE key IN ('api','schema_version')").all() as Row[]).map(r => [r.key, r.value]));
      if (!schemaReadable(String(meta.api), String(meta.schema_version))) {
        this.handle.close();
        this.handle = null;
        throw new Error(`Capture API ${meta.api}, schema ${meta.schema_version} is not readable by this viewer`);
      }
    }
    if (this.idle) clearTimeout(this.idle);
    this.idle = setTimeout(() => this.close(), IDLE_MS);
    return this.handle;
  }

  private schema() {
    const db = this.db; // opened first, so a replaced file's schema is read afresh
    this.features ??= {
      targetEvent: db.query("PRAGMA table_info(targets)").all().some((r: any) => r.name === "event"),
      results: !!db.query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'results'").get(),
    };
    return this.features;
  }

  /** Older captures' targets table has no `event` column (the event their readback followed). */
  get targetEventColumn(): boolean {
    return this.schema().targetEvent;
  }

  /** Captures before the `results` table (what order-dependent shaders wrote) lack it. */
  get hasResults(): boolean {
    return this.schema().results;
  }

  all(sql: string, ...params: SQLQueryBindings[]): Row[] {
    return this.db.query(sql).all(...params) as Row[];
  }

  one(sql: string, ...params: SQLQueryBindings[]): Row | null {
    return (this.db.query(sql).get(...params) as Row | null) ?? null;
  }

  count(sql: string, ...params: SQLQueryBindings[]): number {
    const r = this.db.query(sql).values(...params)[0];
    return r ? Number(r[0]) : 0;
  }

  meta(): Record<string, any> {
    const out: Record<string, any> = {};
    for (const r of this.all("SELECT key, value FROM meta")) out[r.key] = json(r.value);
    return out;
  }

  /** Where a blob is: in a pack (the store's `index.db` says where), or a file of its own (stores from before packs). */
  private locate(key: string): { path: string; at: number; size: number } | null {
    const index = join(this.blobs, "index.db");
    if (!this.blobIndex && existsSync(index)) this.blobIndex = new Database(index, { readonly: true });
    const row = this.blobIndex?.query("SELECT pack, offset, size FROM blobs WHERE key = ?").get(key) as any;
    if (row) return { path: join(this.blobs, "packs", row.pack), at: Number(row.offset), size: Number(row.size) };
    const file = join(this.blobs, key.slice(0, 2), key);
    return existsSync(file) ? { path: file, at: 0, size: statSync(file).size } : null;
  }

  hasBlob(key: string): boolean {
    return this.locate(key) !== null;
  }

  blobSize(key: string): number | null {
    return this.locate(key)?.size ?? null;
  }

  /** `size` bytes of a blob from `offset` (all of it without a size). */
  readBlob(key: string, offset = 0, size?: number): Uint8Array<ArrayBuffer> {
    const where = this.locate(key);
    if (!where) throw new Error(`blob ${key} is not in ${this.blobs}`);
    const total = where.size;
    const n = Math.max(0, Math.min(size ?? total - offset, total - offset));
    const out = new Uint8Array(n);
    const fd = openSync(where.path, "r");
    try {
      let done = 0;
      while (done < n) {
        const got = readSync(fd, out, done, n - done, where.at + offset + done);
        if (got <= 0) break;
        done += got;
      }
    } finally {
      closeSync(fd);
    }
    return out;
  }

  // ---- info -------------------------------------------------------------------------------------------------------

  info() {
    const meta = this.meta();
    const counts = meta.counts ?? {};
    const kinds = this.all("SELECT kind, count(*) AS n FROM events GROUP BY kind ORDER BY n DESC");
    const sources = this.all(
      "SELECT coalesce(content_source, 'none') AS source, count(*) AS n FROM resources WHERE used = 1 GROUP BY content_source");
    const uploads = this.count("SELECT count(*) FROM resources WHERE upload IS NOT NULL");
    const stored = this.count("SELECT count(*) FROM resources WHERE upload_stored = 1");
    const missing: string[] = [];
    for (const g of this.all("SELECT what, detail, count FROM gaps ORDER BY what"))
      missing.push(`${g.what}: ${g.detail ?? ""} (${g.count})`);
    for (const r of this.all(
      "SELECT coalesce(content_note, 'not read') AS why, count(*) AS n FROM resources WHERE used = 1 AND content IS NULL GROUP BY content_note"))
      missing.push(`contents of ${r.n} used resources: ${r.why}`);
    for (const r of this.all(
      "SELECT content_note AS why, count(*) AS n FROM resources WHERE content IS NOT NULL AND content_note IS NOT NULL GROUP BY content_note"))
      missing.push(`contents of ${r.n} resources may be off: ${r.why}`);
    const keys = new Set<string>();
    for (const sql of [
      "SELECT content AS k FROM resources WHERE content IS NOT NULL",
      "SELECT upload AS k FROM resources WHERE upload_stored = 1",
      "SELECT blob AS k FROM targets WHERE blob IS NOT NULL",
      "SELECT data AS k FROM events WHERE data IS NOT NULL",
      "SELECT blob AS k FROM shaders WHERE blob IS NOT NULL",
    ]) for (const r of this.all(sql)) keys.add(r.k);
    const storeExists = existsSync(this.blobs);
    let absent = 0;
    if (storeExists) for (const k of keys) if (!this.hasBlob(k)) absent++;
    if (!storeExists) missing.push(`the blob store ${this.blobs} does not exist (open with --blobs, or set it when opening)`);
    else if (absent > 0) missing.push(`${absent} of ${keys.size} blobs it refers to are not in the store`);
    const memory = meta.memory;
    let memoryLine: string | null = null;
    if (memory?.path) {
      if (existsSync(memory.path)) {
        memoryLine = `${memory.path} (${(statSync(memory.path).size / 1048576).toFixed(1)} MiB; the game waited ${memory.held_ms} ms for the snapshot)`;
      } else if (existsSync(`${memory.path}.part`)) missing.push(`memory: ${memory.path} is still being written, or its writer died`);
      else missing.push(`memory: ${memory.path} is gone`);
    } else if (meta.request?.memory === true && memory?.error == null) {
      missing.push("memory: not dumped (no dumper set: settings.txt `dumper`)");
    }
    return {
      id: this.id,
      path: this.path,
      // The capture's own name, read now: the file may have been replaced since it was opened.
      label: meta.name != null ? String(meta.name) : this.label,
      blobs: this.blobs,
      storeExists,
      targetEvents: this.targetEventColumn,
      meta,
      timeline: {
        events: counts.events ?? this.count("SELECT count(*) FROM events"),
        kinds: kinds.map(k => [k.kind, k.n]),
        draws: counts.draws ?? this.count("SELECT count(*) FROM draws"),
        passes: this.count("SELECT count(*) FROM passes"),
      },
      lists: {
        submissions: meta.api === "d3d12" ? this.count("SELECT count(*) FROM d12_submissions") : null,
        executions: meta.api === "d3d12" ? this.count("SELECT count(*) FROM d12_executions") : null,
        generations: meta.api === "d3d12" ? this.count("SELECT count(*) FROM d12_lists") : null,
        spliced: counts.lists_spliced ?? 0,
        recordedBefore: counts.lists_recorded_before ?? 0,
        unrecorded: counts.lists_unrecorded ?? 0,
      },
      constants: { distinct: this.count("SELECT count(*) FROM cb_data"), failures: counts.cb_copy_failures ?? 0 },
      resources: {
        live: counts.resources_live ?? this.count("SELECT count(*) FROM resources"),
        used: counts.resources_used ?? this.count("SELECT count(*) FROM resources WHERE used = 1"),
        written: counts.resources_written ?? this.count("SELECT count(*) FROM resources WHERE written = 1"),
        contents: counts.contents ?? 0,
        contentBytes: counts.content_bytes ?? 0,
        sources: sources.map(s => [s.source, s.n]),
      },
      uploads: { total: uploads, stored, hashOnly: uploads - stored,
        none: this.count("SELECT count(*) FROM resources WHERE upload IS NULL") },
      // D3D12 captures before the target counts went into meta have only the rows: no size or skips.
      targets: { count: counts.targets ?? this.count("SELECT count(*) FROM targets"), bytes: counts.target_bytes ?? null,
        skipped: counts.targets_skipped ?? null },
      shaders: { count: this.count("SELECT count(*) FROM shaders"),
        withBytecode: this.count("SELECT count(*) FROM shaders WHERE blob IS NOT NULL") },
      blobsReferenced: keys.size,
      blobsAbsent: storeExists ? absent : keys.size,
      memory: memoryLine,
      missing,
      tables: this.all("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").map(r => r.name),
    };
  }

  // ---- timeline ---------------------------------------------------------------------------------------------------

  /** Pass labels: the targets its first draw had bound. */
  private passTargets(): Map<number, string> {
    const out = new Map<number, string[]>();
    const rows = this.all(`
      SELECT p.n AS pass, b.kind, b.slot, v.format AS vformat, r.width, r.height, r.format
      FROM passes p JOIN bindings b ON b.draw = p.first_draw AND b.kind IN ('rtv', 'dsv')
      LEFT JOIN views v ON v.id = b.view LEFT JOIN resources r ON r.id = b.resource
      ORDER BY p.n, b.kind DESC, b.slot`);
    for (const r of rows) {
      const name = r.kind === "dsv" ? "depth" : `rt${r.slot}`;
      const f = formatName(r.vformat ?? r.format);
      const list = out.get(r.pass) ?? [];
      list.push(`${name} ${r.width}x${r.height} ${f}`);
      out.set(r.pass, list);
    }
    return new Map([...out].map(([k, v]) => [k, v.join(", ")]));
  }

  /**
   * Every event in GPU order, each placed in a group: a draw in its pass, a dispatch in a run of compute work, and any
   * other event in the group of the draw or dispatch after it (a pass's clears come before its draws), except events
   * up to where a pass's targets were read back, which belong to that pass.
   */
  timeline(): { rows: TimelineRow[]; groups: Group[] } {
    if (this.timelineCache) return this.timelineCache;
    const rows = this.all(`
      SELECT e.seq, e.kind, e.call, e.context, e.deferred, e.list, e.draw, e.args, e.data,
             d.pass, d.vs, d.hs, d.ds, d.gs, d.ps, d.cs
      FROM events e LEFT JOIN draws d ON d.n = e.draw ORDER BY e.seq`) as TimelineRow[];
    // Key: the pass number, or -1 for compute.
    const own: (number | null)[] = rows.map(r =>
      r.draw == null ? null : r.pass != null ? Number(r.pass) : -1);
    const key: number[] = new Array(rows.length);
    let next = -2;
    for (let i = rows.length - 1; i >= 0; i--) {
      if (own[i] != null) next = own[i]!;
      key[i] = next;
    }
    // Events after a pass's last draw, up to its readback, stay in the pass.
    if (this.targetEventColumn) {
      const ends = this.all(`
        SELECT p.n AS pass, d.event AS last, max(t.event) AS readback
        FROM passes p JOIN draws d ON d.n = p.last_draw LEFT JOIN targets t ON t.draw = p.last_draw
        GROUP BY p.n`);
      const index = new Map(rows.map((r, i) => [r.seq, i]));
      for (const e of ends) {
        if (e.readback == null) continue;
        const from = index.get(e.last), to = index.get(e.readback);
        if (from == null || to == null) continue;
        for (let i = from + 1; i <= to; i++) if (own[i] == null) key[i] = e.pass;
      }
    }
    // Events after the frame's last draw or dispatch join the group before them.
    for (let i = 1; i < rows.length; i++) if (key[i] === -2 && key[i - 1] !== -2) key[i] = key[i - 1]!;
    const labels = this.passTargets();
    const groups: Group[] = [];
    for (let i = 0; i < rows.length; i++) {
      const k = key[i]!;
      if (i === 0 || k !== key[i - 1]) {
        const pass = k >= 0 ? k : null;
        groups.push({
          pass,
          label: pass != null ? `pass ${pass}${labels.has(pass) ? `: ${labels.get(pass)}` : ""}` : k === -1 ? "compute" : "no draws",
          first: i,
          count: 0,
        });
      }
      groups[groups.length - 1]!.count++;
      rows[i]!.group = groups.length - 1;
      rows[i]!.pass = k >= 0 ? k : null;
    }
    this.timelineCache = { rows, groups };
    return this.timelineCache;
  }

  /** The resources each event read and wrote: [seq, resource, access] for one resource, or for every event. */
  eventsTouching(resource: number): { seq: number; access: string }[] {
    return this.all(
      "SELECT event AS seq, access FROM event_resources WHERE resource = ? ORDER BY event", resource) as any;
  }

  // ---- stacks -----------------------------------------------------------------------------------------------------

  private modules() {
    if (!this.modulesCache) {
      this.modulesCache = this.all("SELECT path, base, size FROM modules").map(m => ({
        path: m.path, base: BigInt(m.base), size: BigInt(m.size),
      }));
    }
    return this.modulesCache;
  }

  /** A stack's frames as `module+0xoffset`, innermost first. */
  stack(id: number | null): string[] {
    if (id == null) return [];
    const r = this.one("SELECT frames FROM stacks WHERE id = ?", id);
    if (!r) return [];
    const b = r.frames as Uint8Array;
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
    const out: string[] = [];
    for (let at = 0; at + 8 <= b.length; at += 8) {
      const a = v.getBigUint64(at, true);
      const m = this.modules().find(m => a >= m.base && a < m.base + m.size);
      out.push(m ? `${basename(m.path.replaceAll("\\", "/"))}+0x${(a - m.base).toString(16)}` : `0x${a.toString(16)}`);
    }
    return out;
  }

  // ---- events and draws -------------------------------------------------------------------------------------------

  private shapes(ids: number[]): Map<number, Shape> {
    const out = new Map<number, Shape>();
    const unique = [...new Set(ids.filter(i => i != null))];
    for (let i = 0; i < unique.length; i += 500) {
      const part = unique.slice(i, i + 500);
      for (const r of this.all(`SELECT ${SHAPE} FROM resources WHERE id IN (${part.map(() => "?").join(",")})`, ...part))
        out.set(r.id, shapeOf(r));
    }
    return out;
  }

  private resourceRef(s: Shape | undefined, id: number) {
    if (!s) return { id, text: `#${id} (not alive at the capture)` };
    return { id, text: describeShape(s), kind: s.kind, format: s.format, width: s.width, height: s.height };
  }

  /**
   * What an event whose shader's writes depend on the GPU's thread order wrote through its UAVs, as the game's GPU
   * wrote it: each resource's whole contents right after the event (canonical layout).
   */
  results(seq: number) {
    if (!this.hasResults) return [];
    const rows = this.all("SELECT resource, content FROM results WHERE event = ? ORDER BY resource", seq);
    const shapes = this.shapes(rows.map(r => r.resource));
    return rows.map(r => {
      const s = shapes.get(r.resource);
      return {
        resource: this.resourceRef(s, r.resource), content: r.content as string,
        size: this.blobSize(r.content), stride: s?.stride ?? 0,
      };
    });
  }

  /** One event: its row, what it read and wrote, its stack, and its draw when it is one. */
  private execution(seq: number): string | null {
    if (this.meta().api !== "d3d12") return null;
    const r = this.one("SELECT l.queue,l.execution,x.list,x.generation,l.command_index FROM d12_event_locations l JOIN d12_executions x ON x.id=l.execution WHERE l.event=?",seq);
    return r ? `queue ${r.queue}, execution ${r.execution}, list ${r.list} generation ${r.generation}, command ${r.command_index}` : "D3D12 queue operation";
  }

  event(seq: number) {
    const e = this.one("SELECT * FROM events WHERE seq = ?", seq);
    if (!e) return null;
    const touched = this.all("SELECT resource, access FROM event_resources WHERE event = ?", seq);
    const shapes = this.shapes(touched.map(t => t.resource));
    const { rows } = this.timeline();
    const row = rows.find(r => r.seq === seq);
    let data: { key: string; size: number | null } | null = null;
    if (e.data) data = { key: e.data, size: this.blobSize(e.data) };
    // Views named in the arguments (clears).
    const args = json(e.args);
    let view: any = null;
    if (args && typeof args === "object" && "view" in args) {
      const v = this.one("SELECT * FROM views WHERE id = ?", args.view);
      if (v) view = { id: v.id, kind: v.kind, resource: v.resource, format: formatName(v.format), json: json(v.json) };
    }
    return {
      ...e,
      execution: this.execution(seq),
      args,
      pass: row?.pass ?? null,
      group: row?.group ?? null,
      reads: touched.filter(t => t.access === "read").map(t => this.resourceRef(shapes.get(t.resource), t.resource)),
      writes: touched.filter(t => t.access === "write").map(t => this.resourceRef(shapes.get(t.resource), t.resource)),
      stackFrames: this.stack(e.stack),
      dataBlob: data,
      view,
      results: this.results(seq),
    };
  }

  private shaderReflection(hash: string | null): ConstantBuffer[] | null {
    if (!hash) return null;
    if (!this.reflection.has(hash)) {
      let cbs: ConstantBuffer[] | null = null;
      const r = this.one("SELECT blob FROM shaders WHERE hash = ? AND blob IS NOT NULL", hash);
      if (r && this.hasBlob(r.blob)) cbs = constantBuffers(this.readBlob(r.blob));
      this.reflection.set(hash, cbs);
    }
    return this.reflection.get(hash)!;
  }

  /** A constant buffer's bytes, register by register, with names and types from the shader's reflection. */
  constants(stage: string, shader: string | null, slot: number, space: number, content: string | null) {
    if (!content) return { content: null, reflection: null, registers: [], note: "contents not captured" };
    const r = this.one("SELECT bytes FROM cb_data WHERE hash = ?", content);
    if (!r) return { content, reflection: null, registers: [], note: `contents ${content} missing` };
    const bytes = r.bytes as Uint8Array;
    const cb = this.shaderReflection(shader)?.find(b => b.slot === slot && b.space === space) ?? null;
    const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const registers = [];
    for (let at = 0; at < bytes.length; at += 16) {
      const words: number[] = [], names: (string | null)[] = [], bases: (number | null)[] = [];
      let used = cb == null;
      for (let i = 0; i < 4; i++) {
        const o = at + i * 4;
        words.push(o + 4 <= bytes.length ? v.getUint32(o, true) : 0);
        const n = cb ? nameAt(cb, o) : null;
        names.push(n?.name ?? null);
        bases.push(n?.base ?? null);
        if (n?.used) used = true;
      }
      registers.push({ c: at / 16, words, names, bases, used });
    }
    return {
      content, size: bytes.length, stage, slot,
      reflection: cb ? { name: cb.name, size: cb.size, variables: cb.variables.map(v => ({
        name: v.name, offset: v.offset, size: v.size, used: v.used, type: v.type })) } : null,
      registers,
    };
  }

  /** One draw (or dispatch): its event, shaders, state, every binding, constants by name, and its pass's targets. */
  draw(n: number) {
    const d = this.one("SELECT * FROM draws WHERE n = ?", n);
    if (!d) return null;
    const event = this.one("SELECT * FROM events WHERE seq = ?", d.event);
    const stateIds = [d.layout, d.rasterizer, d.blend, d.depth_stencil].filter(x => x != null);
    const states = new Map<number, Row>();
    if (stateIds.length)
      for (const s of this.all(`SELECT * FROM states WHERE id IN (${stateIds.map(() => "?").join(",")})`, ...stateIds))
        states.set(s.id, s);
    const state = (id: number | null) => (id == null ? null : { id, json: json(states.get(id)?.json ?? null),
      signature: states.get(id)?.signature ?? null });

    const bindings = this.all(`
      SELECT b.*, v.kind AS view_kind, v.format AS view_format, v.dimension AS view_dimension, v.json AS view_json,
             s.json AS state_json
      FROM bindings b LEFT JOIN views v ON v.id = b.view LEFT JOIN states s ON s.id = b.state
      WHERE b.draw = ?`, n);
    bindings.sort((a, b) => (STAGE_ORDER[a.stage] ?? 9) - (STAGE_ORDER[b.stage] ?? 9)
      || (KIND_ORDER[a.kind] ?? 9) - (KIND_ORDER[b.kind] ?? 9) || a.slot - b.slot);
    const resources = bindings.map(b => b.resource).filter(r => r != null);
    const shapes = this.shapes(resources);
    const extra = new Map<number, Row>();
    for (let i = 0; i < resources.length; i += 500) {
      const part = [...new Set(resources.slice(i, i + 500))];
      for (const r of this.all(
        `SELECT id, content, content_source, content_note, upload, upload_stored FROM resources WHERE id IN (${part.map(() => "?").join(",")})`, ...part))
        extra.set(r.id, r);
    }
    // The last event before this draw that wrote each bound resource.
    const lastWrite = new Map<number, number>();
    if (resources.length) {
      const unique = [...new Set(resources)];
      for (const r of this.all(`
        SELECT resource, max(event) AS seq FROM event_resources
        WHERE access = 'write' AND event < ? AND resource IN (${unique.map(() => "?").join(",")})
        GROUP BY resource`, d.event, ...unique)) lastWrite.set(r.resource, r.seq);
    }
    const shaders: Record<string, string | null> = { vs: d.vs, hs: d.hs, ds: d.ds, gs: d.gs, ps: d.ps, cs: d.cs };
    const out = bindings.map(b => {
      const r = b.resource != null ? extra.get(b.resource) : undefined;
      const stage = b.stage as string;
      return {
        stage, kind: b.kind, slot: b.slot, space: b.space,
        view: b.view != null ? { id: b.view, kind: b.view_kind, format: b.view_format, formatName: formatName(b.view_format),
          dimension: b.view_dimension, json: json(b.view_json) } : null,
        resource: b.resource != null ? {
          ...this.resourceRef(shapes.get(b.resource), b.resource),
          content: r?.content ?? null, contentSource: r?.content_source ?? null, contentNote: r?.content_note ?? null,
          upload: r?.upload ?? null, uploadStored: !!r?.upload_stored,
          lastWrite: lastWrite.get(b.resource) ?? null,
        } : null,
        state: b.state != null ? { id: b.state, json: json(b.state_json) } : null,
        offset: b.offset, size: b.size, stride: b.stride, format: b.format,
        formatName: b.format != null ? formatName(b.format) : null,
        content: b.content,
        constants: b.kind === "cb" ? this.constants(stage, shaders[stage] ?? null, b.slot, b.space, b.content) : undefined,
      };
    });

    const pass = d.pass != null ? this.one("SELECT * FROM passes WHERE n = ?", d.pass) : null;
    const prev = this.one("SELECT n, event FROM draws WHERE n < ? ORDER BY n DESC LIMIT 1", n);
    const next = this.one("SELECT n, event FROM draws WHERE n > ? ORDER BY n LIMIT 1", n);
    return {
      n: d.n, event: d.event, call: d.call, args: json(d.args), pass: d.pass,
      eventRow: event ? { ...event, execution: this.execution(d.event), args: json(event.args), stackFrames: this.stack(event.stack) } : null,
      d3d12: this.meta().api === "d3d12" ? this.one("SELECT s.pso,s.graphics_root,s.compute_root,json(s.root_args_json) AS arguments,p.description AS pipeline FROM d12_draw_state s LEFT JOIN d12_psos p ON p.id=s.pso WHERE s.draw=?",n) : null,
      shaders,
      topology: d.topology,
      layout: state(d.layout), rasterizer: state(d.rasterizer), blend: state(d.blend),
      depthStencil: state(d.depth_stencil),
      blendFactor: json(d.blend_factor), sampleMask: d.sample_mask, stencilRef: d.stencil_ref,
      predicate: d.predicate, predicateValue: d.predicate_value,
      viewports: this.all("SELECT * FROM viewports WHERE draw = ? ORDER BY i", n),
      scissors: this.all("SELECT * FROM scissors WHERE draw = ? ORDER BY i", n),
      bindings: out,
      results: this.results(d.event),
      passRange: pass ? { n: pass.n, first: pass.first_draw, last: pass.last_draw } : null,
      targets: pass ? this.targetsAfter(pass.last_draw) : this.targetsAfter(n),
      prev, next,
    };
  }

  /** The targets read back after draw `draw` (a pass's last), each with what it looked like before the pass. */
  targetsAfter(draw: number) {
    const rows = this.all("SELECT * FROM targets WHERE draw = ? ORDER BY target", draw);
    const pass = this.one("SELECT * FROM passes WHERE last_draw = ?", draw);
    const firstEvent = pass ? this.one("SELECT event FROM draws WHERE n = ?", pass.first_draw)?.event : null;
    return rows.map(t => {
      // Before the pass: the latest readback of the same resource after an earlier pass, else its contents as the
      // frame began. Events that wrote it in between (a pass's clear, a copy) are counted, since the picture misses them.
      const bound = pass?.first_draw ?? draw;
      const earlier = this.one(
        `SELECT draw, target, ${this.targetEventColumn ? "event" : "NULL AS event"} FROM targets
         WHERE resource = ? AND sub = ? AND draw < ? ORDER BY draw DESC LIMIT 1`,
        t.resource, t.sub, bound);
      const since = earlier
        ? earlier.event ?? this.one("SELECT event FROM draws WHERE n = ?", earlier.draw)?.event ?? -1
        : -1;
      const writesBetween = firstEvent != null ? this.count(
        "SELECT count(*) FROM event_resources WHERE resource = ? AND access = 'write' AND event > ? AND event < ?",
        t.resource, since, firstEvent) : 0;
      const res = this.one("SELECT content, content_source FROM resources WHERE id = ?", t.resource);
      let before: any = null;
      if (earlier) before = { source: { kind: "target", draw: earlier.draw, name: earlier.target }, label: `after pass ending at draw ${earlier.draw}` };
      else if (res?.content) before = { source: { kind: "resource", id: t.resource, sub: t.sub }, label: "contents as the frame began" };
      return {
        draw: t.draw, name: t.target, view: t.view, resource: t.resource, width: t.width, height: t.height,
        format: t.format, formatName: formatName(t.format), sub: t.sub, blob: t.blob, event: t.event ?? null,
        before, writesBetween,
      };
    });
  }

  // ---- resources --------------------------------------------------------------------------------------------------

  resources() {
    return this.db.query(`
      SELECT id, kind, width, height, depth, mips, array, format, samples, bytes, stride, bind, usage, misc,
             used, written, content_source, content IS NOT NULL AS has_content, upload_stored, update_count
      FROM resources ORDER BY id`).values();
  }

  resource(id: number) {
    const r = this.one("SELECT * FROM resources WHERE id = ?", id);
    if (!r) return null;
    const shape = shapeOf(r);
    delete r.desc;
    const subs = this.all("SELECT * FROM subresources WHERE resource = ? ORDER BY sub", id);
    const views = this.all("SELECT id, kind, format, dimension, json FROM views WHERE resource = ? ORDER BY id", id)
      .map(v => ({ ...v, formatName: formatName(v.format), json: json(v.json) }));
    const events = this.all(`
      SELECT er.event AS seq, er.access, e.kind, e.call, e.draw, d.pass
      FROM event_resources er JOIN events e ON e.seq = er.event LEFT JOIN draws d ON d.n = e.draw
      WHERE er.resource = ? ORDER BY er.event`, id);
    const bindings = this.all(`
      SELECT b.draw, b.stage, b.kind, b.slot, b.view FROM bindings b WHERE b.resource = ? ORDER BY b.draw`, id);
    const targets = this.all("SELECT draw, target, sub, blob, width, height, format FROM targets WHERE resource = ? ORDER BY draw", id);
    return {
      ...r,
      text: describeShape(shape),
      formatName: formatName(r.format),
      bindNames: bindFlags(r.bind),
      usageName: ["default", "immutable", "dynamic", "staging"][r.usage] ?? String(r.usage),
      createdStackFrames: this.stack(r.created_stack),
      updatedStackFrames: this.stack(r.updated_stack),
      subresources: subs.length ? subs : (r.kind === "buffer" ? [] : (textureSubs(r.format, shape.width, shape.height,
        shape.depth, shape.mips, shape.array) ?? []).map(s => ({ resource: id, sub: s.index, mip: s.mip, slice: s.slice,
        width: s.width, height: s.height, depth: s.depth, row_bytes: s.rowBytes, rows: s.rows, offset: s.offset,
        size: s.size, hash: null }))),
      views, events, bindings, targets,
      contentPresent: r.content ? this.hasBlob(r.content) : false,
      uploadPresent: r.upload ? this.hasBlob(r.upload) : false,
    };
  }

  // ---- shaders ----------------------------------------------------------------------------------------------------

  shaders() {
    return this.all(`
      SELECT s.hash, s.stage, s.size, s.blob IS NOT NULL AS has_blob,
        (SELECT count(*) FROM draws d WHERE d.vs = s.hash OR d.hs = s.hash OR d.ds = s.hash OR d.gs = s.hash
           OR d.ps = s.hash OR d.cs = s.hash) AS draws
      FROM shaders s ORDER BY draws DESC`);
  }

  shader(hash: string) {
    const s = this.one("SELECT * FROM shaders WHERE hash = ?", hash);
    if (!s) return null;
    const draws = this.all(`SELECT n FROM draws WHERE vs = ?1 OR hs = ?1 OR ds = ?1 OR gs = ?1 OR ps = ?1 OR cs = ?1 ORDER BY n`, hash)
      .map(r => r.n);
    return { ...s, draws, reflection: this.shaderReflection(hash), blobPresent: s.blob ? this.hasBlob(s.blob) : false };
  }

  // ---- pictures and texels ----------------------------------------------------------------------------------------

  /** Where a source's texels are. */
  texels(src: Source): Texels | string {
    if (src.kind === "target") {
      const t = this.one("SELECT * FROM targets WHERE draw = ? AND target = ?", src.draw, src.name);
      if (!t) return `draw ${src.draw} has no ${src.name} read back`;
      if (!t.blob) return "this target was not read back";
      const l = layout(readable(t.format));
      if (!l) return `${formatName(t.format)} has no texel layout`;
      const size = Math.ceil(t.width / l.blockWidth) * l.blockBytes * Math.ceil(t.height / l.blockHeight);
      return { blob: t.blob, offset: 0, size, format: t.format, width: t.width, height: t.height, sliceBytes: size };
    }
    const r = this.one(`SELECT ${SHAPE}, content, upload FROM resources WHERE id = ?`, src.id);
    if (!r) return `no resource #${src.id}`;
    const key = src.kind === "upload" ? r.upload : r.content;
    if (!key) return src.kind === "upload" ? "no stored upload" : "contents not captured";
    const s = shapeOf(r);
    if (s.kind === "buffer") return "a buffer has no texels";
    const subs = textureSubs(s.format, s.width, s.height, s.depth, s.mips, s.array);
    const sub: Sub | undefined = subs?.[src.sub];
    if (!sub) return `no subresource ${src.sub}`;
    const sliceBytes = sub.rowBytes * sub.rows;
    const z = Math.min(src.z ?? 0, sub.depth - 1);
    return { blob: key, offset: sub.offset + z * sliceBytes, size: sliceBytes, format: s.format, width: sub.width,
      height: sub.height, sliceBytes };
  }

  /** The texel at (x, y) of a source, raw and as the format reads it. */
  texel(src: Source, x: number, y: number) {
    const t = this.texels(src);
    if (typeof t === "string") return { error: t };
    if (x < 0 || y < 0 || x >= t.width || y >= t.height) return { error: "outside the image" };
    const l = layout(readable(t.format))!;
    const rowBytes = Math.ceil(t.width / l.blockWidth) * l.blockBytes;
    const rowAt = Math.floor(y / l.blockHeight) * rowBytes;
    // Read just the row of blocks holding it.
    const bytes = this.readBlob(t.blob, t.offset + rowAt, rowBytes);
    const texel = readTexel(t.format, t.width, bytes, x, 0);
    return { x, y, format: formatName(t.format), ...texel };
  }

  /**
   * Which texels differ between two sources of the same size and format, as an RGBA mask: changed texels magenta,
   * unchanged ones a dim grey. Returns the mask and how many changed.
   */
  diff(a: Source, b: Source): { width: number; height: number; rgba: Uint8Array; changed: number; total: number } | string {
    const ta = this.texels(a), tb = this.texels(b);
    if (typeof ta === "string") return `before: ${ta}`;
    if (typeof tb === "string") return `after: ${tb}`;
    if (ta.width !== tb.width || ta.height !== tb.height) return "the two are different sizes";
    const la = layout(readable(ta.format)), lb = layout(readable(tb.format));
    if (!la || !lb || la.blockBytes !== lb.blockBytes || la.blockWidth !== lb.blockWidth)
      return "the two have different texel layouts";
    const ba = this.readBlob(ta.blob, ta.offset, ta.size), bb = this.readBlob(tb.blob, tb.offset, tb.size);
    const { width, height } = ta;
    const bw = la.blockWidth, bh = la.blockHeight, bytes = la.blockBytes;
    const across = Math.ceil(width / bw);
    const rgba = new Uint8Array(width * height * 4);
    let changed = 0;
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const at = (Math.floor(y / bh) * across + Math.floor(x / bw)) * bytes;
        let same = true;
        for (let k = 0; k < bytes; k++) if (ba[at + k] !== bb[at + k]) { same = false; break; }
        const o = (y * width + x) * 4;
        if (same) { rgba[o] = rgba[o + 1] = rgba[o + 2] = 40; } else { rgba[o] = 255; rgba[o + 1] = 0; rgba[o + 2] = 255; changed++; }
        rgba[o + 3] = 255;
      }
    }
    return { width, height, rgba, changed, total: width * height };
  }

  // ---- SQL --------------------------------------------------------------------------------------------------------

  sql(text: string, limit = 2000) {
    const started = performance.now();
    const stmt = this.db.prepare(text);
    try {
      const columns = stmt.columnNames;
      const rows: any[][] = [];
      let truncated = false;
      for (const row of stmt.iterate() as Iterable<Row>) {
        if (rows.length >= limit) { truncated = true; break; }
        rows.push(columns.map(c => cell(row[c])));
      }
      return { columns, rows, truncated, ms: Math.round(performance.now() - started) };
    } finally {
      stmt.finalize();
    }
  }

  /** Closes the database; the next query opens it again. */
  close() {
    if (this.idle) clearTimeout(this.idle);
    this.idle = null;
    this.handle?.close();
    this.handle = null;
  }
}

function cell(v: any): any {
  if (v instanceof Uint8Array) {
    const hex = [...v.subarray(0, 32)].map(b => b.toString(16).padStart(2, "0")).join("");
    return { blob: v.length, hex: hex + (v.length > 32 ? "…" : "") };
  }
  if (typeof v === "bigint") return v.toString();
  return v;
}

const BIND = ["vertex", "index", "constant", "shader_resource", "stream_output", "render_target", "depth_stencil",
  "unordered_access", "decoder", "video_encoder"];

function bindFlags(bind: number | null): string[] {
  if (!bind) return [];
  return BIND.filter((_, i) => bind & (1 << i));
}

/** The capture.db files under `dir` (a few levels deep), newest first. */
export async function listCaptures(dir: string): Promise<{ path: string; name: string; size: number; mtime: number }[]> {
  const out: { path: string; name: string; size: number; mtime: number }[] = [];
  if (!existsSync(dir)) return out;
  const glob = new Bun.Glob("**/capture.db");
  for await (const rel of glob.scan({ cwd: dir, onlyFiles: true, followSymlinks: false })) {
    if (rel.split(/[\\/]/).length > 8 || /node_modules|[\\/]\.git[\\/]/.test(rel)) continue;
    const path = join(dir, rel);
    try {
      const st = statSync(path);
      out.push({ path, name: basename(dirname(path)), size: st.size, mtime: st.mtimeMs });
    } catch {}
    if (out.length >= 500) break;
  }
  out.sort((a, b) => b.mtime - a.mtime);
  return out;
}

/** A short summary of a capture for lists, read without keeping it open. */
export function summarize(path: string) {
  try {
    const db = new Database(path, { readonly: true });
    try {
      const meta: Record<string, any> = {};
      for (const r of db.query("SELECT key, value FROM meta WHERE key IN ('name','frame','exe','counts','time')").all() as Row[])
        meta[r.key] = json(r.value);
      return {
        name: meta.name ?? null, frame: meta.frame ?? null, exe: meta.exe ? basename(String(meta.exe).replaceAll("\\", "/")) : null,
        draws: meta.counts?.draws ?? null, events: meta.counts?.events ?? null, time: meta.time ?? null,
      };
    } finally {
      db.close();
    }
  } catch (e) {
    return { error: String(e) };
  }
}
