// Pictures of targets and textures: zoom, pan, and a pixel picker showing both the picture's 8-bit value and the
// texel's raw value as the capture holds it. Before/after/diff for a pass's targets.

import { useEffect, useRef, useState } from "preact/hooks";
import { get, sourceText, type SourceObj } from "./api";
import { float } from "./util";

export interface Picture {
  id: string;
  src: SourceObj;
  width: number;
  height: number;
  title: string;
  /** What the same texels looked like before (a pass's target before the pass). */
  before?: { src: SourceObj; label: string; writesBetween?: number } | null;
}

export function imageUrl(id: string, src: SourceObj, range = "auto"): string {
  return `/api/c/${id}/image?${new URLSearchParams({ src: sourceText(src), range })}`;
}

interface Texel {
  x: number;
  y: number;
  format?: string;
  channels?: [string, number][];
  raw?: string;
  kind?: string;
  note?: string;
  error?: string;
}

function loadImage(url: string): Promise<HTMLImageElement> {
  return new Promise((ok, fail) => {
    const img = new Image();
    img.onload = () => ok(img);
    img.onerror = async () => {
      try {
        const r = await fetch(url);
        const j = await r.json();
        fail(new Error(j.error ?? `${r.status}`));
      } catch {
        fail(new Error("the picture could not be made"));
      }
    };
    img.src = url;
  });
}

type Mode = "after" | "before" | "diff";

/** A thumbnail that opens the full viewer. */
export function Thumb({ pic, open, size = 160 }: { pic: Picture; open: (p: Picture) => void; size?: number }) {
  const [failed, setFailed] = useState<string | null>(null);
  return (
    <figure class="thumb" style={{ width: `${size}px` }} onClick={() => open(pic)} title="open (zoom, pick pixels)">
      {failed ? <div class="thumb-fail" style={{ height: `${size * 0.6}px` }}>{failed}</div> : (
        <img src={imageUrl(pic.id, pic.src)} loading="lazy" style={{ maxWidth: `${size}px`, maxHeight: `${size}px` }}
          onError={async () => {
            try {
              const r = await fetch(imageUrl(pic.id, pic.src));
              setFailed((await r.json()).error ?? "failed");
            } catch {
              setFailed("failed");
            }
          }} />
      )}
      <figcaption>{pic.title}</figcaption>
    </figure>
  );
}

export function Viewer({ pic, onClose }: { pic: Picture; onClose?: () => void }) {
  const [range, setRange] = useState("auto");
  const [custom, setCustom] = useState("0:1");
  const [mode, setMode] = useState<Mode>("after");
  const [img, setImg] = useState<HTMLImageElement | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [diffInfo, setDiffInfo] = useState<string | null>(null);
  const [view, setView] = useState({ zoom: 0, x: 0, y: 0 });
  const [hover, setHover] = useState<{ x: number; y: number; rgba: number[] } | null>(null);
  const [picked, setPicked] = useState<Texel | null>(null);
  const [hoverTexel, setHoverTexel] = useState<Texel | null>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const wrap = useRef<HTMLDivElement>(null);
  const pixels = useRef<ImageData | null>(null);
  const drag = useRef<{ x: number; y: number; vx: number; vy: number; moved: boolean } | null>(null);
  const hoverReq = useRef(0);

  const src = mode === "before" && pic.before ? pic.before.src : pic.src;
  const effectiveRange = range === "custom" ? custom : range;

  useEffect(() => {
    let live = true;
    setError(null);
    setDiffInfo(null);
    let url: string;
    if (mode === "diff" && pic.before) {
      url = `/api/c/${pic.id}/diff?${new URLSearchParams({ a: sourceText(pic.before.src), b: sourceText(pic.src) })}`;
      fetch(url).then(async r => {
        if (!live) return;
        if (!r.ok) {
          setError((await r.json()).error);
          return;
        }
        const changed = Number(r.headers.get("x-changed")), total = Number(r.headers.get("x-total"));
        setDiffInfo(`${changed.toLocaleString()} of ${total.toLocaleString()} texels changed (${(100 * changed / total).toFixed(2)}%): magenta`);
        const blob = await r.blob();
        const i = await loadImage(URL.createObjectURL(blob));
        if (live) setImg(i);
      }, e => live && setError(String(e)));
    } else {
      url = imageUrl(pic.id, src, effectiveRange);
      loadImage(url).then(i => live && setImg(i), e => live && setError(String(e.message ?? e)));
    }
    return () => {
      live = false;
    };
  }, [pic.id, sourceText(src), effectiveRange, mode]);

  // Keep the picture's pixels for the hover readout.
  useEffect(() => {
    if (!img) return;
    const c = document.createElement("canvas");
    c.width = img.naturalWidth;
    c.height = img.naturalHeight;
    const g = c.getContext("2d", { willReadFrequently: true })!;
    g.drawImage(img, 0, 0);
    pixels.current = g.getImageData(0, 0, c.width, c.height);
  }, [img]);

  // Fit on first load.
  useEffect(() => {
    if (img && view.zoom === 0) fit();
  }, [img]);

  function fit() {
    const w = wrap.current;
    if (!w) return;
    const zoom = Math.min(w.clientWidth / pic.width, w.clientHeight / pic.height);
    setView({ zoom, x: (w.clientWidth - pic.width * zoom) / 2, y: (w.clientHeight - pic.height * zoom) / 2 });
  }

  function actual() {
    const w = wrap.current;
    if (!w) return;
    setView({ zoom: 1, x: (w.clientWidth - pic.width) / 2, y: (w.clientHeight - pic.height) / 2 });
  }

  // Draw.
  useEffect(() => {
    const c = canvas.current, w = wrap.current;
    if (!c || !w || !img) return;
    const dpr = devicePixelRatio || 1;
    c.width = w.clientWidth * dpr;
    c.height = w.clientHeight * dpr;
    c.style.width = `${w.clientWidth}px`;
    c.style.height = `${w.clientHeight}px`;
    const g = c.getContext("2d")!;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, c.width, c.height);
    g.imageSmoothingEnabled = view.zoom < 1;
    // A checkerboard behind, so transparency shows.
    g.fillStyle = "#222";
    g.fillRect(view.x, view.y, pic.width * view.zoom, pic.height * view.zoom);
    g.drawImage(img, view.x, view.y, pic.width * view.zoom, pic.height * view.zoom);
    if (view.zoom >= 24) {
      g.strokeStyle = "rgba(128,128,128,0.35)";
      g.lineWidth = 1;
      const x0 = Math.max(0, Math.floor(-view.x / view.zoom)), x1 = Math.min(pic.width, Math.ceil((w.clientWidth - view.x) / view.zoom));
      const y0 = Math.max(0, Math.floor(-view.y / view.zoom)), y1 = Math.min(pic.height, Math.ceil((w.clientHeight - view.y) / view.zoom));
      g.beginPath();
      for (let x = x0; x <= x1; x++) { g.moveTo(view.x + x * view.zoom + 0.5, view.y + y0 * view.zoom); g.lineTo(view.x + x * view.zoom + 0.5, view.y + y1 * view.zoom); }
      for (let y = y0; y <= y1; y++) { g.moveTo(view.x + x0 * view.zoom, view.y + y * view.zoom + 0.5); g.lineTo(view.x + x1 * view.zoom, view.y + y * view.zoom + 0.5); }
      g.stroke();
    }
    if (picked) {
      const z = Math.max(view.zoom, 1);
      g.strokeStyle = "#ff0";
      g.lineWidth = 2;
      g.strokeRect(view.x + picked.x * view.zoom - 1, view.y + picked.y * view.zoom - 1, z + 2, z + 2);
    }
  }, [img, view, picked]);

  useEffect(() => {
    const ro = new ResizeObserver(() => setView(v => ({ ...v })));
    if (wrap.current) ro.observe(wrap.current);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
      if (e.key === "Escape") onClose?.();
      else if (e.key === "f") fit();
      else if (e.key === "1") actual();
      else if (e.key === "b" && pic.before) setMode(m => (m === "before" ? "after" : "before"));
      else if (e.key === "d" && pic.before) setMode(m => (m === "diff" ? "after" : "diff"));
      else return;
      e.stopPropagation();
      e.preventDefault();
    };
    addEventListener("keydown", k, true);
    return () => removeEventListener("keydown", k, true);
  }, [pic, onClose]);

  function texelAt(e: MouseEvent): { x: number; y: number } | null {
    const r = canvas.current!.getBoundingClientRect();
    const x = Math.floor((e.clientX - r.left - view.x) / view.zoom), y = Math.floor((e.clientY - r.top - view.y) / view.zoom);
    if (x < 0 || y < 0 || x >= pic.width || y >= pic.height) return null;
    return { x, y };
  }

  async function fetchTexel(x: number, y: number): Promise<Texel> {
    const s = mode === "diff" ? pic.src : src;
    return get<Texel>(`c/${pic.id}/texel?${new URLSearchParams({ src: sourceText(s), x: String(x), y: String(y) })}`);
  }

  function onMove(e: MouseEvent) {
    const d = drag.current;
    if (d) {
      if (Math.abs(e.clientX - d.x) + Math.abs(e.clientY - d.y) > 3) d.moved = true;
      setView(v => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
      return;
    }
    const t = texelAt(e);
    if (!t) {
      setHover(null);
      return;
    }
    const p = pixels.current;
    let rgba = [0, 0, 0, 0];
    if (p && p.width === pic.width) {
      const o = (t.y * p.width + t.x) * 4;
      rgba = [p.data[o]!, p.data[o + 1]!, p.data[o + 2]!, p.data[o + 3]!];
    }
    setHover({ ...t, rgba });
    const req = ++hoverReq.current;
    setTimeout(() => {
      if (req !== hoverReq.current) return;
      fetchTexel(t.x, t.y).then(r => req === hoverReq.current && setHoverTexel(r), () => {});
    }, 40);
  }

  function onWheel(e: WheelEvent) {
    e.preventDefault();
    const r = canvas.current!.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    setView(v => {
      const zoom = Math.min(256, Math.max(0.02, v.zoom * (e.deltaY < 0 ? 1.25 : 0.8)));
      return { zoom, x: mx - ((mx - v.x) * zoom) / v.zoom, y: my - ((my - v.y) * zoom) / v.zoom };
    });
  }

  const readout = (t: Texel | null, label: string) => {
    if (!t) return null;
    if (t.error) return <div class="texel"><b>{label}</b> <span class="error-inline">{t.error}</span></div>;
    return (
      <div class="texel">
        <b>{label}</b> ({t.x}, {t.y}) <span class="muted">{t.format}</span>
        {t.channels?.map(([n, v]) => <span class="ch"> {n} <code>{t.kind === "uint" || t.kind === "sint" || n === "stencil" ? v : float(v)}</code></span>)}
        <span class="muted"> raw </span><code>{t.raw}</code>
        {t.note ? <span class="muted"> ({t.note})</span> : null}
      </div>
    );
  };

  return (
    <div class="viewer">
      <div class="viewer-bar">
        <b>{pic.title}</b>
        <span class="muted">{pic.width}x{pic.height}</span>
        {pic.before ? (
          <span class="seg">
            {(["before", "after", "diff"] as Mode[]).map(m => (
              <button class={mode === m ? "on" : ""} onClick={() => setMode(m)} title={m === "before" ? pic.before!.label : m === "diff" ? "texels whose bytes changed (d)" : "(b toggles)"}>{m}</button>
            ))}
          </span>
        ) : null}
        <label>range <select value={range} onChange={e => setRange((e.target as HTMLSelectElement).value)} disabled={mode === "diff"}>
          <option value="auto">auto</option><option value="clamp">clamp</option><option value="tonemap">tonemap</option><option value="custom">lo:hi</option>
        </select></label>
        {range === "custom" ? <input class="small" value={custom} onChange={e => setCustom((e.target as HTMLInputElement).value)} /> : null}
        <button onClick={fit} title="f">fit</button>
        <button onClick={actual} title="1">1:1</button>
        <span class="muted">{view.zoom ? `${(view.zoom * 100).toFixed(view.zoom < 0.1 ? 1 : 0)}%` : ""}</span>
        <a href={imageUrl(pic.id, src, effectiveRange)} target="_blank">png</a>
        {onClose ? <button class="close" onClick={onClose} title="Esc">close</button> : null}
      </div>
      {mode === "before" && pic.before ? <div class="note">before: {pic.before.label}{pic.before.writesBetween ? `; ${pic.before.writesBetween} event(s) wrote it in between (a clear or copy), which this picture does not show` : ""}</div> : null}
      {diffInfo && mode === "diff" ? <div class="note">{diffInfo}</div> : null}
      <div class="viewer-canvas" ref={wrap}
        onWheel={onWheel}
        onMouseDown={e => { drag.current = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y, moved: false }; }}
        onMouseUp={async e => {
          const d = drag.current;
          drag.current = null;
          if (d && !d.moved) {
            const t = texelAt(e);
            if (t) setPicked(await fetchTexel(t.x, t.y));
          }
        }}
        onMouseLeave={() => { drag.current = null; setHover(null); }}
        onMouseMove={onMove}>
        {error ? <div class="error pad">{error}</div> : !img ? <div class="muted pad">decoding…</div> : null}
        <canvas ref={canvas} />
      </div>
      <div class="viewer-readout">
        {hover ? (
          <div class="texel"><b>hover</b> ({hover.x}, {hover.y}) <span class="muted">picture</span> <code>{hover.rgba.join(" ")}</code>
            {hoverTexel && hoverTexel.x === hover.x && hoverTexel.y === hover.y ? readout(hoverTexel, "") : null}</div>
        ) : <div class="texel muted">hover for values; click to pin a texel; wheel zooms, drag pans; f fit, 1 actual size{pic.before ? ", b before, d diff" : ""}</div>}
        {readout(picked, "pinned")}
      </div>
    </div>
  );
}

/** The full viewer over the page. */
export function Lightbox({ pic, onClose }: { pic: Picture | null; onClose: () => void }) {
  if (!pic) return null;
  return (
    <div class="lightbox" onClick={e => e.target === e.currentTarget && onClose()}>
      <div class="lightbox-inner"><Viewer key={sourceText(pic.src)} pic={pic} onClose={onClose} /></div>
    </div>
  );
}
