// Opening a capture: by path on the server's disk, from a list of captures under a folder, or a capture.db the
// browser holds (drag and drop, or the File System Access API picker), which the server keeps a copy of.

import { useState } from "preact/hooks";
import { get, post, upload, type Info } from "./api";
import { go, href, mib, useAsync } from "./util";

interface Status {
  cli: string | null;
  defaultDir: string;
  initial: string | null;
  recent: { id: string; path: string; label: string; blobs: string | null; uploaded?: boolean }[];
}

interface Listed {
  path: string;
  name: string;
  size: number;
  mtime: number;
  frame?: number;
  exe?: string;
  draws?: number;
  events?: number;
  error?: string;
}

export async function openFile(file: File, blobs: string, setBusy: (s: string | null) => void): Promise<void> {
  setBusy(`sending ${file.name} (${mib(file.size)}) to the server…`);
  try {
    const info = await upload(file, file.name, blobs);
    go(href(info.id, "info"));
  } finally {
    setBusy(null);
  }
}

/** Picks a capture.db with the File System Access API where there is one (Chromium, secure contexts), else an input. */
export async function pickFile(): Promise<File | null> {
  const w = window as any;
  if (typeof w.showOpenFilePicker === "function") {
    try {
      const [handle] = await w.showOpenFilePicker({
        id: "locum-capture",
        types: [{ description: "locum capture", accept: { "application/vnd.sqlite3": [".db"] } }],
      });
      return handle ? await handle.getFile() : null;
    } catch (e) {
      if ((e as Error).name === "AbortError") return null;
      throw e;
    }
  }
  return new Promise(ok => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".db";
    input.onchange = () => ok(input.files?.[0] ?? null);
    input.click();
  });
}

export function OpenPage({ busy, setBusy, blobs, setBlobs }: { busy: string | null; setBusy: (s: string | null) => void; blobs: string; setBlobs: (s: string) => void }) {
  const status = useAsync(() => get<Status>("status"), []);
  const [path, setPath] = useState("");
  const [dir, setDir] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const folder = dir ?? status.data?.defaultDir ?? null;
  const list = useAsync(() => (folder ? get<{ dir: string; captures: Listed[] }>(`captures?dir=${encodeURIComponent(folder)}`) : null), [folder]);
  const fsa = typeof (window as any).showOpenFilePicker === "function";

  async function openPath(p: string) {
    setError(null);
    setBusy(`opening ${p}…`);
    try {
      const info = await post<Info>("open", { path: p, blobs: blobs || undefined });
      go(href(info.id, "info"));
    } catch (e) {
      setError(String((e as Error).message ?? e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div class="page open">
      <h2>open a capture</h2>
      {busy ? <div class="note">{busy}</div> : null}
      {error ? <div class="error">{error}</div> : null}
      <div class="open-grid">
        <div class="card">
          <h3>from this browser</h3>
          <p>Drop a <code>capture.db</code> anywhere on the page, or choose one. The server keeps a copy in its cache (read-only) and reads
            pictures from the blob store the capture names, or the one below.</p>
          <button onClick={async () => {
            setError(null);
            try {
              const f = await pickFile();
              if (f) await openFile(f, blobs, setBusy);
            } catch (e) {
              setError(String((e as Error).message ?? e));
            }
          }}>choose capture.db…</button>
          <div class="muted small">{fsa ? "File System Access API available" : `File System Access API not available here${window.isSecureContext ? "" : " (not a secure context: use http://localhost)"}; using a file input`}</div>
        </div>
        <div class="card">
          <h3>from the server's disk</h3>
          <form onSubmit={e => { e.preventDefault(); if (path.trim()) openPath(path.trim()); }}>
            <input class="wide" placeholder="path to capture.db, or its folder" value={path} onInput={e => setPath((e.target as HTMLInputElement).value)} />
            <button type="submit">open</button>
          </form>
        </div>
        <div class="card">
          <h3>blob store</h3>
          <input class="wide" placeholder="default: the capture's own (meta blobs), LOCUM_BLOBS or --blobs" value={blobs} onInput={e => setBlobs((e.target as HTMLInputElement).value)} />
          <div class="muted small">only needed when the store has moved</div>
        </div>
      </div>
      {status.data?.recent.length ? (
        <>
          <h3>opened before</h3>
          <table class="grid">
            <thead><tr><th>capture</th><th>database</th><th /></tr></thead>
            <tbody>{status.data.recent.map(r => <tr><td><a href={href(r.id, "info")}>{r.label}</a></td><td class="mono small">{r.uploaded ? "(from the browser) " : ""}{r.path}</td><td /></tr>)}</tbody>
          </table>
        </>
      ) : null}
      <h3>captures under <input class="wide inline" value={folder ?? ""} onChange={e => setDir((e.target as HTMLInputElement).value)} /></h3>
      {list.error ? <div class="error">{list.error}</div> : list.data ? (
        list.data.captures.length ? (
          <table class="grid">
            <thead><tr><th>name</th><th>frame</th><th>game</th><th>draws</th><th>events</th><th>size</th><th>modified</th></tr></thead>
            <tbody>{list.data.captures.map(c => (
              <tr class="clickable" onClick={() => openPath(c.path)} title={c.path}>
                <td><a href="javascript:void 0">{c.name}</a></td><td>{c.frame ?? ""}</td><td>{c.exe ?? c.error ?? ""}</td><td>{c.draws ?? ""}</td>
                <td>{c.events ?? ""}</td><td>{mib(c.size)}</td><td>{new Date(c.mtime).toLocaleString()}</td>
              </tr>
            ))}</tbody>
          </table>
        ) : <div class="muted">no capture.db under {list.data.dir}</div>
      ) : <div class="muted">looking…</div>}
      {status.data && !status.data.cli ? <div class="warn">locum-cli was not found: pictures and disassembly are off (start the server with --cli or LOCUM_CLI)</div> : null}
    </div>
  );
}
