import { useEffect, useState } from "preact/hooks";
import type { ComponentChildren } from "preact";

// ---- routing: everything the page shows is in the hash, so links, reloads and back work ----------------------------

export interface Route {
  /** The capture's id, or null on the open page. */
  id: string | null;
  /** info, pipeline, resources, shaders, sql */
  tab: string;
  /** The selected item: an event's seq, a resource id, a shader hash. */
  item: string | null;
  params: URLSearchParams;
}

export function parseRoute(hash: string): Route {
  const [path = "", query = ""] = hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  const params = new URLSearchParams(query);
  if (parts[0] !== "c" || !parts[1]) return { id: null, tab: "open", item: null, params };
  return { id: parts[1], tab: parts[2] ?? "info", item: parts[3] ?? null, params };
}

export function href(id: string, tab: string, item?: string | number | null, params?: Record<string, string>): string {
  let h = `#/c/${id}/${tab}`;
  if (item != null) h += `/${encodeURIComponent(String(item))}`;
  if (params && Object.keys(params).length) h += `?${new URLSearchParams(params)}`;
  return h;
}

export function useRoute(): Route {
  const [hash, setHash] = useState(location.hash);
  useEffect(() => {
    const f = () => setHash(location.hash);
    addEventListener("hashchange", f);
    return () => removeEventListener("hashchange", f);
  }, []);
  return parseRoute(hash);
}

export function go(h: string, replace = false) {
  if (replace) history.replaceState(null, "", h);
  else location.hash = h;
  if (replace) dispatchEvent(new HashChangeEvent("hashchange"));
}

// ---- loading ------------------------------------------------------------------------------------------------------

export type Async<T> = { data: T | undefined; error: string | null; loading: boolean };

export function useAsync<T>(f: () => Promise<T> | null, deps: unknown[]): Async<T> {
  const [state, setState] = useState<Async<T>>({ data: undefined, error: null, loading: true });
  useEffect(() => {
    let live = true;
    const p = f();
    if (!p) {
      setState({ data: undefined, error: null, loading: false });
      return;
    }
    setState(s => ({ data: s.data, error: null, loading: true }));
    p.then(
      data => live && setState({ data, error: null, loading: false }),
      e => live && setState({ data: undefined, error: String(e?.message ?? e), loading: false }),
    );
    return () => {
      live = false;
    };
  }, deps);
  return state;
}

export function Loading({ state, children }: { state: Async<any>; children: () => ComponentChildren }) {
  if (state.error) return <div class="error">{state.error}</div>;
  if (state.data === undefined) return <div class="muted pad">loading…</div>;
  return <>{children()}</>;
}

// ---- values -------------------------------------------------------------------------------------------------------

const f32 = new Float32Array(1);
const u32 = new Uint32Array(f32.buffer);
const i32 = new Int32Array(f32.buffer);

export function bitsToFloat(bits: number): number {
  u32[0] = bits >>> 0;
  return f32[0]!;
}

export function bitsToInt(bits: number): number {
  u32[0] = bits >>> 0;
  return i32[0]!;
}

export function hex(bits: number, width = 8): string {
  return `0x${(bits >>> 0).toString(16).padStart(width, "0")}`;
}

/** A float in at most 12 characters; denormals and NaNs (more likely integers or packed bits) as hex. */
export function float(f: number): string {
  if (Number.isNaN(f)) return "NaN";
  if (!Number.isFinite(f)) return f > 0 ? "inf" : "-inf";
  if (f === 0) return Object.is(f, -0) ? "-0" : "0";
  if (Math.abs(f) < 1.1754944e-38) return "denormal";
  const plain = String(Number(f.toPrecision(8)));
  if (plain.length <= 12) return plain;
  return f.toExponential(4);
}

export type ValueMode = "auto" | "float" | "int" | "uint" | "hex";

/** A 4-byte constant shown as `mode` says; auto uses the reflected type, else float with hex for odd bits. */
export function word(bits: number, base: number | null, mode: ValueMode): string {
  switch (mode) {
    case "float": return float(bitsToFloat(bits));
    case "int": return String(bitsToInt(bits));
    case "uint": return String(bits >>> 0);
    case "hex": return hex(bits);
  }
  if (base === 1) return bits !== 0 ? "true" : "false";
  if (base === 2) return String(bitsToInt(bits));
  if (base === 19 || base === 20) return String(bits >>> 0);
  const f = bitsToFloat(bits);
  const e = (bits >>> 23) & 0xff;
  // Denormals and NaNs are rarely meant as floats: integers and packed bits look like that.
  if ((e === 0 && bits !== 0 && bits !== 0x80000000) || e === 0xff) return hex(bits);
  return float(f);
}

export function mib(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1048576).toFixed(1)} MiB`;
}

export function short(hash: string | null | undefined, n = 12): string {
  return hash ? hash.slice(0, n) : "-";
}

/** JSON on one line, compact. */
export function compact(v: unknown): string {
  return JSON.stringify(v, (_, x) => (typeof x === "number" && !Number.isInteger(x) ? Number(x.toPrecision(7)) : x))
    ?.replace(/,"/g, ", \"") ?? "";
}

// ---- Direct3D enums, for reading state descriptions ----------------------------------------------------------------

export const TOPOLOGY: Record<number, string> = {
  0: "undefined", 1: "point list", 2: "line list", 3: "line strip", 4: "triangle list", 5: "triangle strip",
  10: "line list adj", 11: "line strip adj", 12: "triangle list adj", 13: "triangle strip adj",
};
export function topologyName(t: number): string {
  if (t >= 33 && t <= 64) return `${t - 32} control point patch list`;
  return TOPOLOGY[t] ?? String(t);
}

export const CULL: Record<number, string> = { 1: "none", 2: "front", 3: "back" };
export const FILL: Record<number, string> = { 2: "wireframe", 3: "solid" };
export const COMPARISON: Record<number, string> = {
  1: "never", 2: "less", 3: "equal", 4: "less_equal", 5: "greater", 6: "not_equal", 7: "greater_equal", 8: "always",
};
export const BLEND: Record<number, string> = {
  1: "zero", 2: "one", 3: "src_color", 4: "inv_src_color", 5: "src_alpha", 6: "inv_src_alpha", 7: "dest_alpha",
  8: "inv_dest_alpha", 9: "dest_color", 10: "inv_dest_color", 11: "src_alpha_sat", 14: "blend_factor",
  15: "inv_blend_factor", 16: "src1_color", 17: "inv_src1_color", 18: "src1_alpha", 19: "inv_src1_alpha",
};
export const BLEND_OP: Record<number, string> = { 1: "add", 2: "subtract", 3: "rev_subtract", 4: "min", 5: "max" };
export const STENCIL_OP: Record<number, string> = {
  1: "keep", 2: "zero", 3: "replace", 4: "incr_sat", 5: "decr_sat", 6: "invert", 7: "incr", 8: "decr",
};
export const ADDRESS: Record<number, string> = { 1: "wrap", 2: "mirror", 3: "clamp", 4: "border", 5: "mirror_once" };

export function filterName(f: number): string {
  const kinds = ["", "comparison", "minimum", "maximum"];
  const reduction = kinds[(f >> 7) & 3] ?? "";
  if (f & 0x40) return `${reduction ? reduction + " " : ""}anisotropic`;
  const pt = (s: number) => ((f >> s) & 1 ? "linear" : "point");
  return `${reduction ? reduction + " " : ""}min ${pt(4)}, mag ${pt(2)}, mip ${pt(0)}`;
}

export function writeMask(m: number): string {
  if (m === 0) return "none";
  return ["r", "g", "b", "a"].filter((_, i) => m & (1 << i)).join("");
}
