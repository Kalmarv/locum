// Resources: every resource alive at the capture, and one in detail: description, creation stack, what the frame
// did with it, and its contents.

import { useMemo, useRef, useState, useEffect } from "preact/hooks";
import { cached, get } from "./api";
import { Thumb, Viewer, type Picture } from "./image";
import { BytesView, EventLink, KV, Section, Stack, useCtx } from "./parts";
import { href, Loading, mib, short, sourceWords, useAsync, compact } from "./util";
import { formatName } from "../server/format";

type ResRow = [number, string, number, number, number, number, number, number, number, number, number, number, number, number, number, number, string | null, number, number, number];

const ROW = 20;

function shapeText(r: ResRow): string {
  const [, kind, w, h, d, mips, array, format, samples, , stride, , , misc] = r;
  if (kind === "buffer") return `${w} bytes${stride ? ` stride ${stride}` : ""}`;
  let t = `${w}`;
  if (kind !== "texture1d") t += `x${h}`;
  if (kind === "texture3d") t += `x${d}`;
  t += ` ${formatName(format)}`;
  if (mips > 1) t += ` ${mips} mips`;
  if (misc & 4) t += " cube";
  else if (array > 1) t += ` [${array}]`;
  if (samples > 1) t += ` ${samples}x`;
  return t;
}

export function Resources({ item }: { item: string | null }) {
  const { id } = useCtx();
  const list = useAsync(() => cached<ResRow[]>(`c/${id}/resources`), [id]);
  return (
    <div class="split">
      <Loading state={list}>{() => <ResourceList rows={list.data!} selected={item ? Number(item) : null} />}</Loading>
      <div class="inspector">{item ? <ResourceView rid={Number(item)} /> : <div class="muted pad">Select a resource.</div>}</div>
    </div>
  );
}

function ResourceList({ rows, selected }: { rows: ResRow[]; selected: number | null }) {
  const { id } = useCtx();
  const [used, setUsed] = useState(true);
  const [kind, setKind] = useState("");
  const [text, setText] = useState("");
  const [scroll, setScroll] = useState(0);
  const [height, setHeight] = useState(600);
  const el = useRef<HTMLDivElement>(null);
  const shown = useMemo(() => {
    const t = text.trim().toLowerCase();
    return rows.filter(r => (!used || r[14]) && (!kind || r[1] === kind) &&
      (!t || `#${r[0]}`.includes(t) || shapeText(r).toLowerCase().includes(t)));
  }, [rows, used, kind, text]);
  useEffect(() => {
    const e = el.current;
    if (!e) return;
    const ro = new ResizeObserver(() => setHeight(e.clientHeight));
    ro.observe(e);
    return () => ro.disconnect();
  }, []);
  useEffect(() => {
    const e = el.current;
    if (selected == null || !e) return;
    const i = shown.findIndex(r => r[0] === selected);
    if (i >= 0 && (i * ROW < e.scrollTop || i * ROW > e.scrollTop + e.clientHeight - ROW)) e.scrollTop = Math.max(0, i * ROW - 100);
  }, [selected, shown]);
  const first = Math.max(0, Math.floor(scroll / ROW) - 10), last = Math.min(shown.length, Math.ceil((scroll + height) / ROW) + 10);
  return (
    <div class="timeline">
      <div class="filters">
        <div class="filter-fields">
          <label><input type="checkbox" checked={used} onChange={() => setUsed(!used)} /> used by the frame</label>
          <select value={kind} onChange={e => setKind((e.target as HTMLSelectElement).value)}>
            <option value="">every kind</option>
            {["buffer", "texture1d", "texture2d", "texture3d"].map(k => <option value={k}>{k}</option>)}
          </select>
          <input placeholder="#id, size or format" value={text} onInput={e => setText((e.target as HTMLInputElement).value)} />
        </div>
        <div class="muted small">{shown.length.toLocaleString()} of {rows.length.toLocaleString()} resources</div>
      </div>
      <div class="rows" ref={el} onScroll={e => setScroll((e.target as HTMLDivElement).scrollTop)}>
        <div style={{ height: `${shown.length * ROW}px`, position: "relative" }}>
          {shown.slice(first, last).map((r, i) => (
            <a class={`row${r[0] === selected ? " sel" : ""}`} style={{ top: `${(first + i) * ROW}px` }} href={href(id, "resources", r[0])}>
              <span class="seq">#{r[0]}</span>
              <span class="call">{r[1]}</span>
              <span class="sum">{shapeText(r)}</span>
              <span class="ctx" title="w: written by the frame; c: contents captured">{r[15] ? "w" : ""}{r[17] ? "c" : ""}</span>
            </a>
          ))}
        </div>
      </div>
    </div>
  );
}

function ResourceView({ rid }: { rid: number }) {
  const { id, openPicture } = useCtx();
  const r = useAsync(() => get<any>(`c/${id}/resource/${rid}`), [id, rid]);
  const [sub, setSub] = useState(0);
  const [z, setZ] = useState(0);
  const [which, setWhich] = useState<"resource" | "upload">("resource");
  useEffect(() => { setSub(0); setZ(0); setWhich("resource"); }, [rid]);
  return (
    <Loading state={r}>{() => {
      const x = r.data!;
      const subs: any[] = x.subresources;
      const s = subs[sub];
      const hasPicture = x.kind !== "buffer" && (x.content || (x.upload && x.upload_stored));
      const src = which === "upload" || !x.content ? "upload" : "resource";
      const pic: Picture | null = hasPicture && s ? {
        id, src: { kind: src, id: rid, sub, z }, width: s.width, height: s.height,
        title: `#${rid} ${src === "upload" ? "upload" : "contents as the frame began"}, sub ${sub} (mip ${s.mip}, slice ${s.slice}${s.depth > 1 ? `, z ${z}` : ""})`,
      } : null;
      return (
        <div class="detail">
          <h2>{x.text}</h2>
          <KV rows={[
            ["usage / bind", `${x.usageName} / ${x.bindNames.join(", ") || "none"}${x.cpu ? `, cpu 0x${x.cpu.toString(16)}` : ""}${x.misc ? `, misc 0x${x.misc.toString(16)}` : ""}`],
            ["size", `${mib(x.bytes)} (${x.bytes.toLocaleString()} bytes in the canonical layout)`],
            ["created", `frame ${x.created_frame ?? "?"}`],
            ["this frame", `${x.used ? "used" : "not used"}${x.written ? ", written" : ""}`],
            ["contents", x.content ? <span>{short(x.content, 32)} <span class="muted">({sourceWords(x.content_source)}{x.contentPresent ? "" : ", not in the store"})</span>{x.content_note ? <div class="warn">{x.content_note}</div> : null}</span>
              : <span class="muted">{x.content_note ?? "not captured"}</span>],
            ["upload", x.upload ? <span>{short(x.upload, 32)} <span class="muted">({x.upload_stored ? "stored" : "hash only"})</span></span> : <span class="muted">none</span>],
            ["updated", x.update_count ? `${x.update_count} times, last in frame ${x.updated_frame}` : "never"],
          ]} />
          {pic ? (
            <Section title="picture" extra={
              <span class="pic-controls">
                {x.content && x.upload_stored && x.upload !== x.content ? (
                  <span class="seg"><button class={which === "resource" ? "on" : ""} onClick={() => setWhich("resource")}>contents</button><button class={which === "upload" ? "on" : ""} onClick={() => setWhich("upload")}>upload</button></span>
                ) : null}
                {subs.length > 1 ? <select value={sub} onChange={e => setSub(Number((e.target as HTMLSelectElement).value))}>
                  {subs.map(u => <option value={u.sub}>sub {u.sub}: mip {u.mip} slice {u.slice} {u.width}x{u.height}{u.depth > 1 ? `x${u.depth}` : ""}</option>)}
                </select> : null}
                {s && s.depth > 1 ? <input class="small" type="number" min={0} max={s.depth - 1} value={z} onChange={e => setZ(Number((e.target as HTMLInputElement).value))} /> : null}
              </span>
            }>
              <div class="inline-viewer"><Viewer key={`${rid}-${src}-${sub}-${z}`} pic={pic} /></div>
            </Section>
          ) : null}
          {x.kind === "buffer" && (x.content || x.upload_stored) ? (
            <Section title="contents">
              <BytesView blob={x.content && x.contentPresent ? x.content : x.upload} total={x.bytes} stride={x.stride || 16} />
            </Section>
          ) : null}
          <Section title={`events that touched it (${x.events.length})`}>
            {x.events.length ? (
              <table class="small-table">
                <thead><tr><th>event</th><th>access</th><th>call</th><th>draw</th><th>pass</th></tr></thead>
                <tbody>{x.events.map((e: any) => <tr><td><EventLink seq={e.seq}>{e.seq}</EventLink></td><td>{e.access}</td><td>{e.call}</td><td>{e.draw ?? ""}</td><td>{e.pass ?? ""}</td></tr>)}</tbody>
              </table>
            ) : <div class="muted">none</div>}
          </Section>
          <Section title={`bound to draws (${x.bindings.length})`} open={x.bindings.length < 50}>
            <BindingsOf rows={x.bindings} />
          </Section>
          {x.targets.length ? (
            <Section title={`read back after passes (${x.targets.length})`}>
              <div class="thumbs">{x.targets.map((t: any) => <Thumb open={openPicture} size={160} pic={{
                id, src: { kind: "target", draw: t.draw, name: t.target }, width: t.width, height: t.height,
                title: `${t.target} after draw ${t.draw}`,
              }} />)}</div>
            </Section>
          ) : null}
          <Section title={`views (${x.views.length})`} open={false}>
            <table class="small-table"><tbody>{x.views.map((v: any) => <tr><td>#{v.id}</td><td>{v.kind}</td><td>{v.formatName}</td><td class="mono small">{compact(v.json)}</td></tr>)}</tbody></table>
          </Section>
          <Section title="created at"><Stack frames={x.createdStackFrames} /></Section>
          {x.updatedStackFrames.length ? <Section title="last updated at" open={false}><Stack frames={x.updatedStackFrames} /></Section> : null}
          <Section title={`subresources (${subs.length})`} open={false}>
            <table class="small-table"><thead><tr><th>sub</th><th>mip</th><th>slice</th><th>size</th><th>offset</th><th>bytes</th><th>hash</th></tr></thead>
              <tbody>{subs.map(u => <tr><td>{u.sub}</td><td>{u.mip}</td><td>{u.slice}</td><td>{u.width}x{u.height}{u.depth > 1 ? `x${u.depth}` : ""}</td><td>{u.offset}</td><td>{u.size}</td><td class="mono">{short(u.hash, 16)}</td></tr>)}</tbody></table>
          </Section>
        </div>
      );
    }}</Loading>
  );
}

function BindingsOf({ rows }: { rows: any[] }) {
  const { id } = useCtx();
  const timeline = useAsync(() => cached<{ rows: { seq: number; draw: number | null }[] }>(`c/${id}/timeline`), [id]);
  const seqOf = useMemo(() => new Map((timeline.data?.rows ?? []).filter(r => r.draw != null).map(r => [r.draw!, r.seq])), [timeline.data]);
  if (!rows.length) return <div class="muted">none</div>;
  return (
    <div class="chips">
      {rows.map(b => {
        const seq = seqOf.get(b.draw);
        const label = `${b.draw} ${b.stage} ${b.kind}${b.slot}`;
        return seq != null ? <a class="chip" href={href(id, "pipeline", seq)}>{label}</a> : <span class="chip">{label}</span>;
      })}
    </div>
  );
}
