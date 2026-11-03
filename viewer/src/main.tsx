import { render } from "preact";
import { useEffect, useState } from "preact/hooks";
import { cached, type Info } from "./api";
import { Lightbox, type Picture } from "./image";
import { InfoPage } from "./info";
import { OpenPage, openFile } from "./open";
import { CaptureCtx } from "./parts";
import { Pipeline } from "./pipeline";
import { Resources } from "./resources";
import { Shaders } from "./shaders";
import { Sql } from "./sql";
import { href, Loading, useAsync, useRoute, type ValueMode } from "./util";
import "./style.css";

const TABS: [string, string][] = [["info", "info"], ["pipeline", "pipeline"], ["resources", "resources"], ["shaders", "shaders"], ["sql", "sql"]];

function App() {
  const route = useRoute();
  const [busy, setBusy] = useState<string | null>(null);
  const [blobs, setBlobs] = useState(() => localStorage.getItem("locum-blobs") ?? "");
  const [dragging, setDragging] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => localStorage.setItem("locum-blobs", blobs), [blobs]);

  // A capture.db dropped anywhere opens it.
  useEffect(() => {
    const over = (e: DragEvent) => {
      if (!e.dataTransfer?.types.includes("Files")) return;
      e.preventDefault();
      setDragging(true);
    };
    const leave = (e: DragEvent) => { if (!e.relatedTarget) setDragging(false); };
    const drop = async (e: DragEvent) => {
      e.preventDefault();
      setDragging(false);
      const f = e.dataTransfer?.files?.[0];
      if (!f) return;
      setError(null);
      try {
        await openFile(f, blobs, setBusy);
      } catch (err) {
        setError(String((err as Error).message ?? err));
      }
    };
    addEventListener("dragover", over);
    addEventListener("dragleave", leave);
    addEventListener("drop", drop);
    return () => {
      removeEventListener("dragover", over);
      removeEventListener("dragleave", leave);
      removeEventListener("drop", drop);
    };
  }, [blobs]);

  // Start on the capture the server was started with.
  useEffect(() => {
    if (location.hash !== "" && location.hash !== "#") return;
    fetch("/api/status").then(r => r.json()).then(s => { if (s.initial) location.hash = href(s.initial, "info"); });
  }, []);

  return (
    <>
      {dragging ? <div class="dropzone">drop a capture.db to open it</div> : null}
      {busy && route.id ? <div class="busy">{busy}</div> : null}
      {error ? <div class="error top" onClick={() => setError(null)}>{error}</div> : null}
      {route.id ? <CaptureApp key={route.id} id={route.id} tab={route.tab} item={route.item} />
        : <><Header /><OpenPage busy={busy} setBusy={setBusy} blobs={blobs} setBlobs={setBlobs} /></>}
    </>
  );
}

function Header({ info, tab }: { info?: Info; tab?: string }) {
  return (
    <header>
      <a class="brand" href="#/">locum</a>
      {info ? <>
        <span class="cap" title={info.path}>{info.label} <span class="muted">frame {info.meta.frame}</span></span>
        <nav>{TABS.map(([t, label]) => <a class={t === tab ? "on" : ""} href={href(info.id, t)}>{label}</a>)}</nav>
      </> : null}
      <span class="spacer" />
      <a class="muted" href="#/">open…</a>
    </header>
  );
}

function CaptureApp({ id, tab, item }: { id: string; tab: string; item: string | null }) {
  const info = useAsync(() => cached<Info>(`c/${id}/info`), [id]);
  const [mode, setModeState] = useState<ValueMode>(() => (localStorage.getItem("locum-mode") as ValueMode) ?? "auto");
  const [picture, setPicture] = useState<Picture | null>(null);
  const setMode = (m: ValueMode) => { setModeState(m); localStorage.setItem("locum-mode", m); };
  useEffect(() => {
    if (info.data) document.title = `${info.data.label} · locum`;
  }, [info.data]);
  return (
    <CaptureCtx.Provider value={{ id, mode, setMode, openPicture: setPicture }}>
      <Header info={info.data} tab={tab} />
      <main>
        <Loading state={info}>{() => {
          switch (tab) {
            case "pipeline": return <Pipeline seq={item != null ? Number(item) : null} />;
            case "resources": return <Resources item={item} />;
            case "shaders": return <Shaders item={item} />;
            case "sql": return <Sql />;
            default: return <InfoPage info={info.data!} />;
          }
        }}</Loading>
      </main>
      <Lightbox pic={picture} onClose={() => setPicture(null)} />
    </CaptureCtx.Provider>
  );
}

render(<App />, document.getElementById("app")!);
