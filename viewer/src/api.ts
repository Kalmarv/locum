// The server's API, typed as the front end uses it.

export interface Info {
  id: string;
  path: string;
  label: string;
  blobs: string;
  storeExists: boolean;
  targetEvents: boolean;
  meta: Record<string, any>;
  timeline: { events: number; kinds: [string, number][]; draws: number; passes: number };
  lists: { spliced: number; recordedBefore: number; unrecorded: number; submissions: number | null; executions: number | null; generations: number | null };
  constants: { distinct: number; failures: number };
  resources: { live: number; used: number; written: number; contents: number; contentBytes: number; sources: [string, number][] };
  uploads: { total: number; stored: number; hashOnly: number; none: number };
  targets: { count: number; bytes: number | null; skipped: number | null };
  shaders: { count: number; withBytecode: number };
  blobsReferenced: number;
  blobsAbsent: number;
  memory: string | null;
  missing: string[];
  tables: string[];
}

export interface Row {
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
  pass: number | null;
  label: string;
  first: number;
  count: number;
}

export interface ResourceRef {
  id: number;
  text: string;
  kind?: string;
  format?: number;
  width?: number;
  height?: number;
}

export interface Register {
  c: number;
  words: number[];
  names: (string | null)[];
  bases: (number | null)[];
  used: boolean;
}

export interface Constants {
  content: string | null;
  size?: number;
  note?: string;
  reflection: { name: string; size: number; variables: { name: string; offset: number; size: number; used: boolean; type: string }[] } | null;
  registers: Register[];
}

export interface Binding {
  stage: string;
  kind: string;
  slot: number;
  space: number;
  view: { id: number; kind: string; format: number; formatName: string; dimension: number; json: any } | null;
  resource: (ResourceRef & { content: string | null; contentSource: string | null; contentNote: string | null;
    upload: string | null; uploadStored: boolean; lastWrite: number | null }) | null;
  state: { id: number; json: any } | null;
  offset: number | null;
  size: number | null;
  stride: number | null;
  format: number | null;
  formatName: string | null;
  content: string | null;
  constants?: Constants;
}

export interface Target {
  draw: number;
  name: string;
  view: number;
  resource: number;
  width: number;
  height: number;
  format: number;
  formatName: string;
  sub: number;
  blob: string | null;
  event: number | null;
  before: { source: SourceObj; label: string } | null;
  writesBetween: number;
}

export type SourceObj = { kind: "target"; draw: number; name: string } | { kind: "resource" | "upload"; id: number; sub: number; z?: number };

export function sourceText(s: SourceObj): string {
  return s.kind === "target" ? `target:${s.draw}:${s.name}` : `${s.kind}:${s.id}:${s.sub}:${s.z ?? 0}`;
}

/** What an order-dependent draw or dispatch wrote through a UAV, as the game's GPU wrote it. */
export interface ResultOut {
  resource: ResourceRef;
  content: string;
  /** Bytes in the blob store, or null when it is not there. */
  size: number | null;
  stride: number;
}

export interface Draw {
  n: number;
  event: number;
  call: string;
  args: any;
  pass: number | null;
  eventRow: any;
  d3d12: { pso: number; graphics_root: number; compute_root: number; arguments: string; pipeline: string } | null;
  shaders: Record<string, string | null>;
  topology: number;
  layout: { id: number; json: any; signature: string | null } | null;
  rasterizer: { id: number; json: any } | null;
  blend: { id: number; json: any } | null;
  depthStencil: { id: number; json: any } | null;
  blendFactor: number[] | null;
  sampleMask: number | null;
  stencilRef: number | null;
  predicate: number | null;
  predicateValue: number | null;
  viewports: any[];
  scissors: any[];
  bindings: Binding[];
  passRange: { n: number; first: number; last: number } | null;
  targets: Target[];
  results: ResultOut[];
  prev: { n: number; event: number } | null;
  next: { n: number; event: number } | null;
}

export interface EventDetail {
  execution: string | null;
  seq: number;
  kind: string;
  call: string;
  context: number;
  deferred: number;
  list: number | null;
  draw: number | null;
  args: any;
  data: string | null;
  stack: number | null;
  pass: number | null;
  group: number | null;
  reads: ResourceRef[];
  writes: ResourceRef[];
  stackFrames: string[];
  dataBlob: { key: string; size: number | null } | null;
  results: ResultOut[];
  view: { id: number; kind: string; resource: number; format: string; json: any } | null;
}

export class ApiError extends Error {}

async function check(r: Response): Promise<Response> {
  if (!r.ok) {
    let msg = `${r.status} ${r.statusText}`;
    try {
      const j = await r.json();
      if (j?.error) msg = j.error;
    } catch {}
    throw new ApiError(msg);
  }
  return r;
}

export async function get<T>(path: string): Promise<T> {
  return (await check(await fetch(`/api/${path}`))).json();
}

export async function post<T>(path: string, body: unknown): Promise<T> {
  return (await check(await fetch(`/api/${path}`, { method: "POST", body: JSON.stringify(body),
    headers: { "content-type": "application/json" } }))).json();
}

export async function text(path: string): Promise<string> {
  return (await check(await fetch(`/api/${path}`))).text();
}

export async function bytes(path: string): Promise<{ data: Uint8Array; total: number }> {
  const r = await check(await fetch(`/api/${path}`));
  return { data: new Uint8Array(await r.arrayBuffer()), total: Number(r.headers.get("x-blob-size") ?? 0) };
}

/** Sends a capture.db the browser holds to the server, which keeps a copy in its cache and opens it. */
export async function upload(file: Blob, name: string, blobs: string): Promise<Info> {
  const q = new URLSearchParams({ name });
  if (blobs) q.set("blobs", blobs);
  return (await check(await fetch(`/api/upload?${q}`, { method: "POST", body: file }))).json();
}

const memo = new Map<string, Promise<any>>();

/** `get`, remembered for the page's life (captures do not change while open). */
export function cached<T>(path: string): Promise<T> {
  let p = memo.get(path);
  if (!p) {
    p = get<T>(path);
    p.catch(() => memo.delete(path));
    memo.set(path, p);
  }
  return p;
}
