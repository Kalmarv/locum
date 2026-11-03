// Pieces several pages share: links, a bytes view, constant buffers by register, stacks.

import type { ComponentChildren } from "preact";
import { createContext } from "preact";
import { useContext, useState } from "preact/hooks";
import { bytes, type Constants, type ResourceRef } from "./api";
import { bitsToFloat, float, hex, href, useAsync, word, type ValueMode } from "./util";
import { f16 } from "../server/format";

/** The open capture's id, and what the page shares: how constants are shown, and the picture viewer. */
export interface Ctx {
  id: string;
  mode: ValueMode;
  setMode: (m: ValueMode) => void;
  openPicture: (p: import("./image").Picture) => void;
}

export const CaptureCtx = createContext<Ctx>(null as any);
export const useCtx = () => useContext(CaptureCtx);

export function ResLink({ r, children }: { r: ResourceRef | number; children?: ComponentChildren }) {
  const { id } = useCtx();
  const rid = typeof r === "number" ? r : r.id;
  return <a href={href(id, "resources", rid)} title={typeof r === "number" ? undefined : r.text}>{children ?? (typeof r === "number" ? `#${r}` : r.text)}</a>;
}

export function EventLink({ seq, children }: { seq: number; children?: ComponentChildren }) {
  const { id } = useCtx();
  return <a href={href(id, "pipeline", seq)}>{children ?? `event ${seq}`}</a>;
}

export function ShaderLink({ hash }: { hash: string | null }) {
  const { id } = useCtx();
  if (!hash) return <span class="muted">-</span>;
  return <a class="mono" href={href(id, "shaders", hash)}>{hash}</a>;
}

export function ModeSelect() {
  const { mode, setMode } = useCtx();
  return (
    <span class="seg" title="how 4-byte values are shown (auto: by the shader's reflected type, else float)">
      {(["auto", "float", "int", "uint", "hex"] as ValueMode[]).map(m => (
        <button class={mode === m ? "on" : ""} onClick={() => setMode(m)}>{m}</button>
      ))}
    </span>
  );
}

export function Section({ title, open = true, children, extra }: { title: ComponentChildren; open?: boolean; children: ComponentChildren; extra?: ComponentChildren }) {
  return (
    <details class="section" open={open}>
      <summary><span class="section-title">{title}</span>{extra ? <span class="section-extra" onClick={e => e.preventDefault()}>{extra}</span> : null}</summary>
      <div class="section-body">{children}</div>
    </details>
  );
}

export function Stack({ frames }: { frames: string[] }) {
  if (!frames.length) return <span class="muted">no stack recorded</span>;
  return <ol class="stack">{frames.map(f => <li><code>{f}</code></li>)}</ol>;
}

/** A constant buffer register by register: c#, the four values, and their names from the shader's reflection. */
export function ConstantsTable({ title, constants }: { title: ComponentChildren; constants: Constants }) {
  const { mode } = useCtx();
  const [all, setAll] = useState(false);
  if (!constants.content || constants.note) return <div class="cb"><div class="cb-title">{title}: <span class="muted">{constants.note}</span></div></div>;
  const regs = all || !constants.reflection ? constants.registers : constants.registers.filter(r => r.used);
  const hidden = constants.registers.length - regs.length;
  return (
    <div class="cb">
      <div class="cb-title">
        {title} <span class="muted">({constants.reflection ? constants.reflection.name : "no reflection"}, {constants.size} bytes, contents {constants.content.slice(0, 12)})</span>
        {constants.reflection ? <label class="small-toggle"><input type="checkbox" checked={all} onChange={() => setAll(!all)} /> unused registers{hidden ? ` (${hidden})` : ""}</label> : null}
      </div>
      <table class="regs">
        <tbody>
          {regs.map(r => (
            <tr class={r.used ? "" : "dim"}>
              <td class="reg">c{r.c}</td>
              {r.words.map((w, i) => <td class="val" title={`${hex(w)}  float ${float(bitsToFloat(w))}  int ${w | 0}`}>{word(w, r.bases[i] ?? null, mode)}</td>)}
              <td class="names">{registerName(r.names)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** One name for a register when its components belong to one variable (`world[2]`), else each component's. */
function registerName(names: (string | null)[]): string {
  if (names.every(n => n == null)) return "";
  const stem = (n: string) => n.replace(/(\.[xyzw]|\[\d+\])$/, "");
  const first = names.find(n => n != null)!;
  if (names.every(n => n == null || stem(n) === stem(first)) && names.filter(n => n).length > 1) {
    const parts = names.map(n => (n ? n.slice(stem(n).length) : "_"));
    return `${stem(first)}${parts.every(p => /^\.[xyzw]$/.test(p)) ? "." + parts.map(p => p.slice(1)).join("") : `{${parts.join(" ")}}`}`;
  }
  return names.map(n => n ?? "_").join(", ");
}

type BytesMode = "hex" | "f32" | "u32" | "i32" | "f16" | "u16" | "u8";

/** A blob's bytes, a page at a time, as hex or as numbers. */
export function BytesView({ blob, total, stride: initialStride }: { blob: string; total?: number | null; stride?: number }) {
  const { id } = useCtx();
  const [mode, setMode] = useState<BytesMode>("f32");
  const [offset, setOffset] = useState(0);
  const [stride, setStride] = useState(initialStride && initialStride > 0 && initialStride <= 256 ? initialStride : 16);
  const page = 4096;
  const data = useAsync(() => bytes(`c/${id}/blob/${blob}?offset=${offset}&size=${page}`), [id, blob, offset]);
  const size = data.data?.total ?? total ?? 0;
  const rows: { at: number; cells: string[]; hex: string }[] = [];
  if (data.data) {
    const b = data.data.data;
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
    for (let at = 0; at < b.length; at += stride) {
      const cells: string[] = [];
      const end = Math.min(at + stride, b.length);
      const w = mode === "hex" || mode === "u8" ? 1 : mode === "f16" || mode === "u16" ? 2 : 4;
      if (mode !== "hex") for (let k = at; k + w <= end; k += w) {
        cells.push(mode === "f32" ? float(v.getFloat32(k, true)) : mode === "u32" ? String(v.getUint32(k, true))
          : mode === "i32" ? String(v.getInt32(k, true)) : mode === "f16" ? float(f16(v.getUint16(k, true)))
          : mode === "u16" ? String(v.getUint16(k, true)) : String(v.getUint8(k)));
      }
      rows.push({ at: offset + at, cells, hex: [...b.subarray(at, end)].map(x => x.toString(16).padStart(2, "0")).join(" ") });
    }
  }
  return (
    <div class="bytes">
      <div class="bytes-bar">
        <span class="seg">{(["f32", "u32", "i32", "f16", "u16", "u8", "hex"] as BytesMode[]).map(m => <button class={mode === m ? "on" : ""} onClick={() => setMode(m)}>{m}</button>)}</span>
        <label>row <input class="small" type="number" min={4} max={256} step={4} value={stride} onChange={e => setStride(Math.max(4, Math.min(256, Number((e.target as HTMLInputElement).value) || 16)))} /> bytes</label>
        <button disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - page))}>prev</button>
        <span class="muted">{offset.toLocaleString()}–{Math.min(offset + page, size).toLocaleString()} of {size.toLocaleString()} bytes</span>
        <button disabled={offset + page >= size} onClick={() => setOffset(offset + page)}>next</button>
        <label>at <input class="small" placeholder="offset" onKeyDown={e => {
          if (e.key === "Enter") {
            const t = (e.target as HTMLInputElement).value.trim();
            const n = t.startsWith("0x") ? parseInt(t, 16) : Number(t);
            if (Number.isFinite(n) && n >= 0) setOffset(Math.floor(n / page) * page);
          }
        }} /></label>
        <a href={`/api/c/${id}/blob/${blob}?size=${16 << 20}`} target="_blank" download={blob}>raw</a>
      </div>
      {data.error ? <div class="error">{data.error}</div> : (
        <div class="bytes-rows">
          {rows.map(r => (
            <div class="bytes-row">
              <span class="off">{r.at.toString(16).padStart(8, "0")}</span>
              {mode === "hex" ? <span class="hex">{r.hex}</span> : <span class="nums">{r.cells.map(c => <span class="num">{c}</span>)}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function KV({ rows }: { rows: [ComponentChildren, ComponentChildren][] }) {
  return (
    <table class="kv"><tbody>{rows.filter(r => r[1] !== undefined).map(([k, v]) => <tr><th>{k}</th><td>{v}</td></tr>)}</tbody></table>
  );
}
