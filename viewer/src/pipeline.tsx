// The pipeline: the timeline of every event in GPU order, grouped by pass, and an inspector for the selected one.

import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import { cached, get, type Binding, type Draw, type EventDetail, type Group, type ResultOut, type Row } from "./api";
import { Thumb, type Picture } from "./image";
import { BytesView, ConstantsTable, EventLink, KV, ModeSelect, ResLink, Section, ShaderLink, Stack, useCtx } from "./parts";
import {
  ADDRESS, BLEND, BLEND_OP, COMPARISON, CULL, FILL, STENCIL_OP, compact, filterName, go, href, Loading, short,
  topologyName, useAsync, writeMask,
} from "./util";
import { formatName } from "../server/format";

const ROW = 20;
const KINDS = ["draw", "dispatch", "clear", "copy", "update", "map", "map_read", "resolve", "mips"];

interface Filter {
  kinds: string[] | null;
  shader: string;
  pass: string;
  resource: string;
  text: string;
}

const EMPTY: Filter = { kinds: null, shader: "", pass: "", resource: "", text: "" };

function loadFilter(): Filter {
  try {
    return { ...EMPTY, ...JSON.parse(sessionStorage.getItem("locum-filter") ?? "{}") };
  } catch {
    return EMPTY;
  }
}

/** A one-line summary of what an event did. */
function summary(r: Row): string {
  let a: any = r.args;
  try {
    a = JSON.parse(r.args);
  } catch {}
  switch (r.kind) {
    case "draw":
      return `#${r.draw} ${Array.isArray(a) ? `[${a.join(",")}]` : r.args}  vs ${short(r.vs, 8)} ps ${short(r.ps, 8)}`;
    case "dispatch":
      return `#${r.draw} ${Array.isArray(a) ? `[${a.join(",")}]` : r.args}  cs ${short(r.cs, 8)}`;
    case "update":
      return `→ #${a?.dst} sub ${a?.sub}${a?.box ? ` box ${compact(a.box)}` : ""}`;
    case "map":
    case "map_read":
      return `#${a?.resource} sub ${a?.sub}${a?.ranges ? ` ${a.ranges.map((x: number[]) => `${x[0]}..${x[1]}`).join(" ")}` : ""}`;
    case "copy":
      return `#${a?.src} → #${a?.dst}${a?.dst_sub != null ? ` sub ${a.dst_sub}` : ""}`;
    case "clear":
      return `view #${a?.view}${a?.color ? ` ${compact(a.color)}` : a?.depth != null ? ` depth ${a.depth} stencil ${a.stencil}` : a?.values ? ` ${compact(a.values)}` : ""}`;
  }
  return typeof a === "string" ? a : compact(a);
}

export function Pipeline({ seq }: { seq: number | null }) {
  const { id } = useCtx();
  const timeline = useAsync(() => cached<{ rows: Row[]; groups: Group[] }>(`c/${id}/timeline`), [id]);
  return (
    <div class="pipeline">
      <Loading state={timeline}>{() => <Timeline rows={timeline.data!.rows} groups={timeline.data!.groups} seq={seq} />}</Loading>
      <div class="inspector">
        {seq == null ? <div class="muted pad">Select an event (j/k step through them, J/K draws, [ ] passes).</div>
          : <Inspector seq={seq} rows={timeline.data?.rows ?? null} />}
      </div>
    </div>
  );
}

type Item = { group: number } | { row: Row; index: number };

function Timeline({ rows, groups, seq }: { rows: Row[]; groups: Group[]; seq: number | null }) {
  const { id } = useCtx();
  const [filter, setFilterState] = useState<Filter>(loadFilter);
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const [scroll, setScroll] = useState(0);
  const [height, setHeight] = useState(600);
  const list = useRef<HTMLDivElement>(null);
  const setFilter = (f: Filter) => {
    setFilterState(f);
    sessionStorage.setItem("locum-filter", JSON.stringify(f));
  };

  const resourceId = /^\s*#?\d+\s*$/.test(filter.resource) ? Number(filter.resource.replace("#", "")) : null;
  const touching = useAsync(() => resourceId == null ? null
    : get<{ events: { seq: number; access: string }[] }>(`c/${id}/resource/${resourceId}`), [id, resourceId]);

  const visible = useMemo(() => {
    const kinds = filter.kinds ? new Set(filter.kinds) : null;
    const shader = filter.shader.trim().toLowerCase();
    const passes = filter.pass.trim() ? new Set(filter.pass.split(/[ ,]+/).filter(Boolean).flatMap(p => {
      const m = p.match(/^(\d+)(?:-(\d+))?$/);
      if (!m) return [];
      const a = Number(m[1]), b = m[2] ? Number(m[2]) : a;
      return Array.from({ length: Math.max(0, Math.min(b - a + 1, 10000)) }, (_, i) => a + i);
    })) : null;
    const text = filter.text.trim().toLowerCase();
    const seqs = resourceId != null && touching.data ? new Set(touching.data.events.map(e => e.seq)) : null;
    return rows.filter(r =>
      (!kinds || kinds.has(r.kind)) &&
      (!shader || [r.vs, r.hs, r.ds, r.gs, r.ps, r.cs].some(s => s?.startsWith(shader))) &&
      (!passes || (r.pass != null && passes.has(r.pass))) &&
      (!seqs || seqs.has(r.seq)) &&
      (!text || r.call.toLowerCase().includes(text) || r.args.toLowerCase().includes(text) || (r.data ?? "").startsWith(text)));
  }, [rows, filter, touching.data]);

  const items = useMemo(() => {
    const out: Item[] = [];
    let last = -1;
    visible.forEach((row, index) => {
      if (row.group !== last) {
        out.push({ group: row.group });
        last = row.group;
      }
      if (!collapsed.has(row.group)) out.push({ row, index });
    });
    return out;
  }, [visible, collapsed]);

  const position = useMemo(() => {
    const m = new Map<number, number>();
    items.forEach((it, i) => "row" in it && m.set(it.row.seq, i));
    return m;
  }, [items]);

  useEffect(() => {
    const el = list.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Keep the selection in view.
  useEffect(() => {
    const el = list.current;
    if (seq == null || !el) return;
    const i = position.get(seq);
    if (i == null) return;
    const top = i * ROW;
    if (top < el.scrollTop + ROW) el.scrollTop = Math.max(0, top - ROW * 3);
    else if (top > el.scrollTop + el.clientHeight - ROW * 2) el.scrollTop = top - el.clientHeight + ROW * 4;
  }, [seq, position]);

  // Keyboard stepping through what the filter shows.
  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t instanceof HTMLSelectElement) return;
      if (e.ctrlKey || e.metaKey || e.altKey || document.querySelector(".lightbox")) return;
      const at = seq == null ? -1 : visible.findIndex(r => r.seq === seq);
      let next: Row | undefined;
      const find = (from: number, step: number, ok: (r: Row) => boolean) => {
        for (let i = from + step; i >= 0 && i < visible.length; i += step) if (ok(visible[i]!)) return visible[i];
        return undefined;
      };
      const cur = at >= 0 ? visible[at] : undefined;
      switch (e.key) {
        case "j": case "ArrowDown": next = visible[Math.min(at + 1, visible.length - 1)]; break;
        case "k": case "ArrowUp": next = visible[Math.max(at - 1, 0)]; break;
        case "J": next = find(at, 1, r => r.draw != null); break;
        case "K": next = find(at, -1, r => r.draw != null); break;
        case "]": next = find(at, 1, r => r.group !== cur?.group); break;
        case "[": {
          const first = find(at, -1, r => r.group !== cur?.group);
          next = first ? visible.find(r => r.group === first.group) : undefined;
          break;
        }
        case "Home": next = visible[0]; break;
        case "End": next = visible[visible.length - 1]; break;
        case "PageDown": next = visible[Math.min(at + 20, visible.length - 1)]; break;
        case "PageUp": next = visible[Math.max(at - 20, 0)]; break;
        default: return;
      }
      e.preventDefault();
      if (next) go(href(id, "pipeline", next.seq), true);
    };
    addEventListener("keydown", k);
    return () => removeEventListener("keydown", k);
  }, [visible, seq, id]);

  const first = Math.max(0, Math.floor(scroll / ROW) - 10);
  const last = Math.min(items.length, Math.ceil((scroll + height) / ROW) + 10);
  const kinds = filter.kinds ?? KINDS;
  const present = useMemo(() => new Set(rows.map(r => r.kind)), [rows]);

  return (
    <div class="timeline">
      <div class="filters">
        <div class="kinds">
          {KINDS.filter(k => present.has(k)).map(k => (
            <label class={`kind k-${k}`}><input type="checkbox" checked={kinds.includes(k)} onChange={() => {
              const s = new Set(kinds);
              s.has(k) ? s.delete(k) : s.add(k);
              setFilter({ ...filter, kinds: s.size === KINDS.length ? null : [...s] });
            }} />{k}</label>
          ))}
          <button class="link" onClick={() => setFilter({ ...filter, kinds: ["draw", "dispatch"] })}>draws only</button>
          <button class="link" onClick={() => setFilter({ ...filter, kinds: null })}>all</button>
        </div>
        <div class="filter-fields">
          <input placeholder="shader hash" value={filter.shader} onInput={e => setFilter({ ...filter, shader: (e.target as HTMLInputElement).value })} />
          <input placeholder="pass (3, 5-9)" value={filter.pass} onInput={e => setFilter({ ...filter, pass: (e.target as HTMLInputElement).value })} />
          <input placeholder="resource #" value={filter.resource} onInput={e => setFilter({ ...filter, resource: (e.target as HTMLInputElement).value })} />
          <input placeholder="call / args / data" value={filter.text} onInput={e => setFilter({ ...filter, text: (e.target as HTMLInputElement).value })} />
          {JSON.stringify(filter) !== JSON.stringify(EMPTY) ? <button class="link" onClick={() => setFilter(EMPTY)}>clear</button> : null}
        </div>
        <div class="muted small">
          {visible.length.toLocaleString()} of {rows.length.toLocaleString()} events
          {touching.loading && resourceId != null ? " (finding the events touching it…)" : ""}
          {" · "}<button class="link" onClick={() => setCollapsed(new Set(groups.map((_, i) => i)))}>collapse</button>
          {" "}<button class="link" onClick={() => setCollapsed(new Set())}>expand</button>
        </div>
      </div>
      <div class="rows" ref={list} onScroll={e => setScroll((e.target as HTMLDivElement).scrollTop)}>
        <div style={{ height: `${items.length * ROW}px`, position: "relative" }}>
          {items.slice(first, last).map((it, i) => {
            const top = (first + i) * ROW;
            if ("group" in it) {
              const g = groups[it.group]!;
              const shown = visible.filter(r => r.group === it.group).length;
              return (
                <div class="row group" style={{ top: `${top}px` }} title={g.label} onClick={() => {
                  const s = new Set(collapsed);
                  s.has(it.group) ? s.delete(it.group) : s.add(it.group);
                  setCollapsed(s);
                }}>
                  <span class="twisty">{collapsed.has(it.group) ? "▸" : "▾"}</span> {g.label} <span class="muted">({shown})</span>
                </div>
              );
            }
            const r = it.row;
            return (
              <a class={`row k-${r.kind}${r.seq === seq ? " sel" : ""}`} style={{ top: `${top}px` }} href={href(id, "pipeline", r.seq)}>
                <span class="seq">{r.seq}</span>
                <span class="call">{r.call}</span>
                <span class="sum">{summary(r)}</span>
                {r.deferred ? <span class="ctx" title={`recorded on context ${r.context}, command list ${r.list}`}>L{r.list}</span> : null}
              </a>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function Inspector({ seq, rows }: { seq: number; rows: Row[] | null }) {
  const { id } = useCtx();
  const row = rows?.find(r => r.seq === seq);
  const ev = useAsync(() => get<EventDetail>(`c/${id}/event/${seq}`), [id, seq]);
  const drawN = row?.draw ?? ev.data?.draw ?? null;
  const index = rows ? rows.findIndex(r => r.seq === seq) : -1;
  const prev = index > 0 ? rows![index - 1] : null, next = rows && index >= 0 && index < rows.length - 1 ? rows[index + 1] : null;
  return (
    <div>
      <div class="insp-nav">
        {prev ? <a href={href(id, "pipeline", prev.seq)}>← event {prev.seq}</a> : <span />}
        <span class="muted">event {seq}{row?.pass != null ? `, pass ${row.pass}` : ""}</span>
        {next ? <a href={href(id, "pipeline", next.seq)}>event {next.seq} →</a> : <span />}
      </div>
      {drawN != null ? <DrawView n={drawN} /> : <Loading state={ev}>{() => <EventView e={ev.data!} />}</Loading>}
    </div>
  );
}

function EventView({ e }: { e: EventDetail }) {
  return (
    <div class="detail">
      <h2>{e.call} <span class="muted">{e.kind}, event {e.seq}</span></h2>
      <KV rows={[
        ["context", e.deferred ? `deferred context ${e.context}, command list ${e.list} (placed where it was executed)` : `immediate (${e.context})`],
        ["pass", e.pass ?? "-"],
        ["arguments", <code class="wrap">{typeof e.args === "string" ? e.args : JSON.stringify(e.args, null, 1)}</code>],
        ...(e.view ? [["view", <span>#{e.view.id} {e.view.kind} {e.view.format} of <ResLink r={e.view.resource} /></span>] as [string, any]] : []),
        ["read", e.reads.length ? e.reads.map(r => <div><ResLink r={r} /></div>) : <span class="muted">nothing</span>],
        ["wrote", e.writes.length ? e.writes.map(r => <div><ResLink r={r} /></div>) : <span class="muted">nothing</span>],
      ]} />
      {e.dataBlob ? (
        <Section title={`bytes written (${e.dataBlob.size?.toLocaleString() ?? "missing from the store"} bytes, ${short(e.dataBlob.key)})`}>
          {e.dataBlob.size != null ? <BytesView blob={e.dataBlob.key} total={e.dataBlob.size} /> : <div class="muted">not in the blob store</div>}
        </Section>
      ) : null}
      <Results results={e.results} />
      <Section title="call stack" open={false}><Stack frames={e.stackFrames} /></Section>
    </div>
  );
}

/** Output of a shader whose writes depend on the GPU's thread order (atomics), recorded right after the event. */
function Results({ results }: { results: ResultOut[] }) {
  if (!results.length) return null;
  return (
    <Section title={`order-dependent: output recorded for ${results.map(r => `#${r.resource.id}`).join(", ")}`}>
      <div class="muted small">its shader's writes depend on the order the GPU runs threads in, so the capture keeps what
        the game's GPU wrote; replay says how its own output compared (identical, the same records in another order, or
        different in more than order) and then puts these bytes in place and goes on from them</div>
      {results.map(r => (
        <div class="result-out">
          <div><ResLink r={r.resource} /> <span class="muted">contents right after it, {short(r.content, 16)}{r.size == null ? ", not in the blob store" : `, ${r.size.toLocaleString()} bytes`}</span></div>
          {r.size != null ? <BytesView blob={r.content} total={r.size} stride={r.stride || 16} /> : null}
        </div>
      ))}
    </Section>
  );
}

const STAGES = ["vs", "hs", "ds", "gs", "ps", "cs"];

export function DrawView({ n }: { n: number }) {
  const { id, openPicture } = useCtx();
  const d = useAsync(() => get<Draw>(`c/${id}/draw/${n}`), [id, n]);
  return (
    <Loading state={d}>{() => {
      const draw = d.data!;
      const byStage = new Map<string, Binding[]>();
      for (const b of draw.bindings) byStage.set(b.stage, [...(byStage.get(b.stage) ?? []), b]);
      const isCompute = !!draw.shaders.cs;
      const textures = draw.bindings.filter(b => (b.kind === "srv" || b.kind === "uav") && b.resource?.kind && b.resource.kind !== "buffer" && b.resource.content);
      const pics: Picture[] = draw.targets.map(t => ({
        id, src: { kind: "target", draw: t.draw, name: t.name }, width: t.width, height: t.height,
        title: `${t.name} after pass ${draw.pass ?? ""} (draw ${t.draw}): #${t.resource} ${t.formatName}`,
        before: t.before ? { src: t.before.source, label: t.before.label, writesBetween: t.writesBetween } : null,
      }));
      return (
        <div class="detail">
          <h2>
            {isCompute ? "dispatch" : "draw"} {draw.n} <span class="muted">{draw.call} {compact(draw.args)}</span>
          </h2>
          <div class="insp-nav">
            {draw.prev ? <a href={href(id, "pipeline", draw.prev.event)}>← draw {draw.prev.n}</a> : <span />}
            <span class="muted">
              event <EventLink seq={draw.event}>{draw.event}</EventLink>
              {draw.passRange ? <>, pass {draw.passRange.n} (draws {draw.passRange.first}–{draw.passRange.last}{draw.passRange.last === draw.n ? ", its last" : ""})</> : null}
              {draw.eventRow?.deferred ? `, command list ${draw.eventRow.list} (recorded on context ${draw.eventRow.context})` : ", immediate context"}
            </span>
            {draw.next ? <a href={href(id, "pipeline", draw.next.event)}>draw {draw.next.n} →</a> : <span />}
          </div>

          <Section title="shaders">
            <KV rows={STAGES.filter(s => draw.shaders[s]).map(s => [s, <ShaderLink hash={draw.shaders[s]!} />])} />
          </Section>

          {!isCompute ? <PipelineState draw={draw} /> : null}

          <Section title={`bindings (${draw.bindings.length})`}>
            <table class="bindings">
              <thead><tr><th>stage</th><th>slot</th><th>view</th><th>resource</th><th>contents</th><th>last written</th></tr></thead>
              <tbody>
                {[...byStage].flatMap(([stage, bs]) => bs.map((b, i) => <BindingRow b={b} first={i === 0} stage={stage} />))}
              </tbody>
            </table>
          </Section>

          <Section title="constants" extra={<ModeSelect />}>
            {draw.bindings.filter(b => b.kind === "cb").map(b => (
              <ConstantsTable title={<span><b>{b.stage} b{b.slot}</b>{b.resource ? <> <ResLink r={b.resource}>#{b.resource.id}</ResLink></> : null}{b.offset ? ` @${b.offset}` : ""}</span>} constants={b.constants!} />
            ))}
            {!draw.bindings.some(b => b.kind === "cb") ? <div class="muted">no constant buffers bound</div> : null}
          </Section>

          <Section title={pics.length ? `targets read back after the pass (draw ${draw.targets[0]?.draw})` : "targets"}>
            {pics.length ? <div class="thumbs">{pics.map(p => <Thumb pic={p} open={openPicture} size={220} />)}</div>
              : <div class="muted">{draw.passRange ? "none were read back for this pass" : "a dispatch has no targets; see its UAVs above"}</div>}
          </Section>

          {textures.length ? (
            <Section title={`textures it reads (${textures.length}), contents as the frame began`}>
              <div class="thumbs">
                {textures.map(b => {
                  const r = b.resource!;
                  const mip = Number(b.view?.json?.range?.first_mip ?? b.view?.json?.range?.mip ?? 0) || 0;
                  const w = Math.max(1, (r.width ?? 1) >> mip), h = Math.max(1, (r.height ?? 1) >> mip);
                  return <Thumb open={openPicture} size={120} pic={{
                    id, src: { kind: "resource", id: r.id, sub: mip }, width: w, height: h,
                    title: `${b.stage} ${b.kind}${b.slot} #${r.id} ${b.view?.formatName ?? ""}${mip ? ` mip ${mip}` : ""}`,
                  }} />;
                })}
              </div>
            </Section>
          ) : null}

          <Results results={draw.results} />

          <Section title="call stack" open={false}><Stack frames={draw.eventRow?.stackFrames ?? []} /></Section>
        </div>
      );
    }}</Loading>
  );
}

function BindingRow({ b, first, stage }: { b: Binding; first: boolean; stage: string }) {
  const r = b.resource;
  let view = "";
  if (b.view) view = `#${b.view.id} ${b.view.json?.dimension ?? ""} ${b.view.formatName} ${b.view.json?.range ? compact(b.view.json.range) : ""}`;
  else if (b.kind === "vb") view = `offset ${b.offset} stride ${b.stride}`;
  else if (b.kind === "ib") view = `${b.formatName} offset ${b.offset}`;
  else if (b.kind === "cb") view = `offset ${b.offset ?? 0} size ${b.size}`;
  else if (b.kind === "so") view = `offset ${b.offset}`;
  return (
    <tr class={first ? "stage-first" : ""}>
      <td class="stage">{first ? stage : ""}</td>
      <td class="slot">{b.kind}{b.slot}{b.space ? ` space${b.space}` : ""}</td>
      <td>{b.kind === "sampler" && b.state ? <SamplerText s={b.state.json} /> : view}</td>
      <td>{r ? <ResLink r={r} /> : b.kind === "sampler" ? <span class="muted">#{b.state?.id}</span> : <span class="muted">-</span>}</td>
      <td>{r ? (r.content ? <span title={r.contentNote ?? ""}>{short(r.content)} <span class="muted">({r.contentSource})</span>{r.contentNote ? " ⚠" : ""}</span>
        : <span class="muted">{r.contentNote ?? "not captured"}</span>) : null}</td>
      <td>{r?.lastWrite != null ? <EventLink seq={r.lastWrite}>{r.lastWrite}</EventLink> : <span class="muted">{r ? "not this frame" : ""}</span>}</td>
    </tr>
  );
}

function SamplerText({ s }: { s: any }) {
  if (!s) return null;
  return <span title={compact(s)}>{filterName(s.filter)}; {s.address?.map((a: number) => ADDRESS[a] ?? a).join("/")}{s.compare && s.filter & 0x80 ? `; compare ${COMPARISON[s.compare]}` : ""}{s.max_anisotropy > 1 ? `; aniso ${s.max_anisotropy}` : ""}{s.mip_lod_bias ? `; bias ${s.mip_lod_bias}` : ""}</span>;
}

function PipelineState({ draw }: { draw: Draw }) {
  const rs = draw.rasterizer?.json, bl = draw.blend?.json, ds = draw.depthStencil?.json, layout = draw.layout?.json;
  return (
    <Section title="pipeline state">
      <div class="state-grid">
        <div>
          <h4>input assembly</h4>
          <KV rows={[["topology", `${topologyName(draw.topology)} (${draw.topology})`], ["layout", draw.layout ? `#${draw.layout.id}` : "none"]]} />
          {Array.isArray(layout) && layout.length ? (
            <table class="small-table">
              <thead><tr><th>semantic</th><th>format</th><th>slot</th><th>offset</th><th>rate</th></tr></thead>
              <tbody>{layout.map((e: any) => <tr><td>{e.semantic}{e.index ? e.index : ""}</td><td>{formatName(e.format)}</td><td>{e.slot}</td><td>{e.offset}</td><td>{e.per_instance ? `instance/${e.step}` : "vertex"}</td></tr>)}</tbody>
            </table>
          ) : null}
        </div>
        <div>
          <h4>rasterizer</h4>
          {rs ? <KV rows={[
            ["fill / cull", `${FILL[rs.fill] ?? rs.fill} / ${CULL[rs.cull] ?? rs.cull}${rs.front_ccw ? ", front ccw" : ""}`],
            ["depth bias", `${rs.depth_bias}, clamp ${rs.depth_bias_clamp}, slope ${rs.slope_scaled_depth_bias}`],
            ["depth clip / scissor", `${rs.depth_clip} / ${rs.scissor}`],
            ["msaa / aa lines", `${rs.multisample} / ${rs.antialiased_line}`],
          ]} /> : <span class="muted">default</span>}
          <h4>viewports</h4>
          {draw.viewports.map(v => <div class="mono small">{v.i}: {v.x},{v.y} {v.width}x{v.height} depth {v.min_depth}..{v.max_depth}</div>)}
          {draw.scissors.length ? <><h4>scissors</h4>{draw.scissors.map(s => <div class="mono small">{s.i}: ({s.left}, {s.top})-({s.right}, {s.bottom})</div>)}</> : null}
        </div>
        <div>
          <h4>depth-stencil</h4>
          {ds ? <KV rows={[
            ["depth", ds.depth ? `test ${COMPARISON[ds.func] ?? ds.func}, write ${ds.write}` : "off"],
            ["stencil", ds.stencil ? `ref ${draw.stencilRef}, read ${ds.read_mask} write ${ds.write_mask}` : "off"],
            ...(ds.stencil ? [
              ["front", `${COMPARISON[ds.front.func]}: pass ${STENCIL_OP[ds.front.pass]}, fail ${STENCIL_OP[ds.front.fail]}, depth fail ${STENCIL_OP[ds.front.depth_fail]}`],
              ["back", `${COMPARISON[ds.back.func]}: pass ${STENCIL_OP[ds.back.pass]}, fail ${STENCIL_OP[ds.back.fail]}, depth fail ${STENCIL_OP[ds.back.depth_fail]}`],
            ] as [string, string][] : []),
          ]} /> : <span class="muted">default</span>}
          <h4>blend</h4>
          <KV rows={[["factor / sample mask", `${compact(draw.blendFactor)} / 0x${((draw.sampleMask ?? 0xffffffff) >>> 0).toString(16)}`],
            ["alpha to coverage / independent", bl ? `${bl.alpha_to_coverage} / ${bl.independent}` : "-"]]} />
          {bl ? (
            <table class="small-table">
              <thead><tr><th>rt</th><th>colour</th><th>alpha</th><th>write</th></tr></thead>
              <tbody>{(bl.independent ? bl.targets : bl.targets.slice(0, 1)).map((t: any, i: number) => (
                <tr><td>{bl.independent ? i : "all"}</td>
                  <td>{t.blend ? `${BLEND[t.src]} ${BLEND_OP[t.op]} ${BLEND[t.dst]}` : "off"}</td>
                  <td>{t.blend ? `${BLEND[t.src_alpha]} ${BLEND_OP[t.op_alpha]} ${BLEND[t.dst_alpha]}` : ""}</td>
                  <td>{writeMask(t.write_mask)}</td></tr>
              ))}</tbody>
            </table>
          ) : null}
          {draw.predicate != null ? <KV rows={[["predicate", `#${draw.predicate} = ${draw.predicateValue}`]]} /> : null}
        </div>
      </div>
    </Section>
  );
}
