// Shaders: the ones the frame used, each with its reflection, the draws that ran it, and its disassembly.

import { useMemo, useState } from "preact/hooks";
import { cached, get, text } from "./api";
import { KV, Section, useCtx } from "./parts";
import { href, Loading, mib, useAsync } from "./util";

interface ShaderRow {
  hash: string;
  stage: string;
  size: number;
  has_blob: number;
  draws: number;
}

export function Shaders({ item }: { item: string | null }) {
  const { id } = useCtx();
  const list = useAsync(() => cached<ShaderRow[]>(`c/${id}/shaders`), [id]);
  const [stage, setStage] = useState("");
  const [q, setQ] = useState("");
  const shown = useMemo(() => (list.data ?? []).filter(s => (!stage || s.stage === stage) && s.hash.startsWith(q.trim().toLowerCase())), [list.data, stage, q]);
  return (
    <div class="split">
      <div class="timeline">
        <div class="filters">
          <div class="filter-fields">
            <select value={stage} onChange={e => setStage((e.target as HTMLSelectElement).value)}>
              <option value="">every stage</option>{["vs", "hs", "ds", "gs", "ps", "cs"].map(s => <option value={s}>{s}</option>)}
            </select>
            <input placeholder="hash" value={q} onInput={e => setQ((e.target as HTMLInputElement).value)} />
          </div>
          <div class="muted small">{shown.length} shaders</div>
        </div>
        <div class="rows plain">
          <Loading state={list}>{() => shown.map(s => (
            <a class={`row static${s.hash === item ? " sel" : ""}`} href={href(id, "shaders", s.hash)}>
              <span class="call">{s.stage}</span><span class="seq mono">{s.hash}</span><span class="sum">{s.draws} draws</span>
            </a>
          ))}</Loading>
        </div>
      </div>
      <div class="inspector">{item ? <ShaderView hash={item} /> : <div class="muted pad">Select a shader.</div>}</div>
    </div>
  );
}

function ShaderView({ hash }: { hash: string }) {
  const { id } = useCtx();
  const s = useAsync(() => get<any>(`c/${id}/shader/${hash}`), [id, hash]);
  const [show, setShow] = useState(false);
  const timeline = useAsync(() => cached<{ rows: { seq: number; draw: number | null }[] }>(`c/${id}/timeline`), [id]);
  const seqOf = useMemo(() => new Map((timeline.data?.rows ?? []).filter(r => r.draw != null).map(r => [r.draw!, r.seq])), [timeline.data]);
  return (
    <Loading state={s}>{() => {
      const x = s.data!;
      return (
        <div class="detail">
          <h2>{x.stage} {x.hash}</h2>
          <KV rows={[["bytecode", x.blob ? `${mib(x.size ?? 0)}, blob ${x.blob}${x.blobPresent ? "" : " (not in the store)"}` : "not captured"],
            ["draws", `${x.draws.length}`]]} />
          <Section title={`draws that ran it (${x.draws.length})`} open={x.draws.length <= 200}>
            <div class="chips">{x.draws.map((n: number) => seqOf.has(n) ? <a class="chip" href={href(id, "pipeline", seqOf.get(n)!)}>{n}</a> : <span class="chip">{n}</span>)}</div>
          </Section>
          <Section title="constant buffers (reflection)">
            {x.reflection ? x.reflection.map((cb: any) => (
              <div class="cb">
                <div class="cb-title"><b>{cb.name}</b> <span class="muted">{cb.slot != null ? `b${cb.slot}${cb.space ? ` space${cb.space}` : ""}` : "not bound"}, {cb.size} bytes</span></div>
                <table class="small-table">
                  <thead><tr><th>offset</th><th>size</th><th>type</th><th>name</th><th>used</th></tr></thead>
                  <tbody>{cb.variables.map((v: any) => <tr class={v.used ? "" : "dim"}><td>{v.offset}</td><td>{v.size}</td><td>{v.type}</td><td>{v.name}</td><td>{v.used ? "yes" : ""}</td></tr>)}</tbody>
                </table>
              </div>
            )) : <div class="muted">no reflection (the bytecode has no RDEF chunk, or it was stripped)</div>}
          </Section>
          <Section title="disassembly" extra={!show ? <button onClick={() => setShow(true)}>disassemble</button> : null}>
            {show ? <Disassembly hash={hash} /> : <div class="muted">locum-cli disassembles it on request</div>}
          </Section>
        </div>
      );
    }}</Loading>
  );
}

function Disassembly({ hash }: { hash: string }) {
  const { id } = useCtx();
  const d = useAsync(() => text(`c/${id}/shader/${hash}/disasm`), [id, hash]);
  return <Loading state={d}>{() => <pre class="disasm">{d.data}</pre>}</Loading>;
}
