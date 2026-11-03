// A read-only SQL console over the capture.db, with the schema beside it.

import { useEffect, useRef, useState } from "preact/hooks";
import { post } from "./api";
import { href, useAsync } from "./util";
import { useCtx } from "./parts";

interface Result {
  columns: string[];
  rows: any[][];
  truncated: boolean;
  ms: number;
}

const EXAMPLES: [string, string][] = [
  ["draws per pass", "SELECT pass, count(*) AS draws, min(n) AS first, max(n) AS last FROM draws GROUP BY pass ORDER BY pass"],
  ["most used pixel shaders", "SELECT ps, count(*) AS draws FROM draws WHERE ps IS NOT NULL GROUP BY ps ORDER BY draws DESC LIMIT 50"],
  ["largest textures used", "SELECT id, width, height, mips, format, bytes FROM resources WHERE used = 1 AND kind != 'buffer' ORDER BY bytes DESC LIMIT 50"],
  ["what wrote a resource", "SELECT e.seq, e.kind, e.call, e.draw FROM event_resources er JOIN events e ON e.seq = er.event WHERE er.resource = 1 AND er.access = 'write'"],
  ["textures each draw reads", "SELECT b.draw, b.stage, b.slot, b.resource, r.width, r.height, r.format FROM bindings b JOIN resources r ON r.id = b.resource WHERE b.kind = 'srv' AND r.kind != 'buffer' LIMIT 200"],
  ["gaps", "SELECT * FROM gaps"],
];

export function Sql() {
  const { id } = useCtx();
  const [sql, setSql] = useState(() => sessionStorage.getItem("locum-sql") ?? EXAMPLES[0]![1]);
  const [result, setResult] = useState<Result | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const schema = useAsync(() => post<Result>(`c/${id}/sql`, { sql: "SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name" }), [id]);

  async function run(text = sql) {
    setBusy(true);
    setError(null);
    sessionStorage.setItem("locum-sql", text);
    try {
      setResult(await post<Result>(`c/${id}/sql`, { sql: text }));
    } catch (e) {
      setError(String((e as Error).message ?? e));
      setResult(null);
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => { area.current?.focus(); }, []);

  return (
    <div class="sql">
      <div class="sql-main">
        <textarea ref={area} value={sql} spellcheck={false} onInput={e => setSql((e.target as HTMLTextAreaElement).value)}
          onKeyDown={e => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); run(); } }} />
        <div class="sql-bar">
          <button onClick={() => run()} disabled={busy}>run (Ctrl+Enter)</button>
          <span class="muted">the database is opened read-only; at most 2,000 rows are shown</span>
          {result ? <span class="muted">{result.rows.length.toLocaleString()} rows{result.truncated ? " (truncated)" : ""} in {result.ms} ms</span> : null}
        </div>
        {error ? <div class="error">{error}</div> : null}
        {result ? <ResultTable r={result} id={id} /> : null}
      </div>
      <div class="sql-side">
        <h4>examples</h4>
        {EXAMPLES.map(([name, q]) => <div><button class="link" onClick={() => { setSql(q); run(q); }}>{name}</button></div>)}
        <h4>tables</h4>
        {schema.data?.rows.map(([name, def]) => (
          <details><summary><button class="link" onClick={e => { e.preventDefault(); const q = `SELECT * FROM ${name} LIMIT 100`; setSql(q); run(q); }}>{name}</button></summary><pre class="schema">{def}</pre></details>
        ))}
      </div>
    </div>
  );
}

/** Links for columns whose values name something the viewer can show. */
function cell(col: string, v: any, id: string) {
  if (v && typeof v === "object" && "blob" in v) return <span class="muted" title={v.hex}>blob {v.blob} B</span>;
  if (v == null) return <span class="muted">NULL</span>;
  const c = col.toLowerCase();
  if (typeof v === "number" && ["resource", "dst", "src", "resource_id"].includes(c)) return <a href={href(id, "resources", v)}>{v}</a>;
  if (typeof v === "number" && ["seq", "event"].includes(c)) return <a href={href(id, "pipeline", v)}>{v}</a>;
  if (typeof v === "string" && /^(vs|hs|ds|gs|ps|cs|hash)$/.test(c) && /^[0-9a-f]{16}$/.test(v)) return <a href={href(id, "shaders", v)}>{v}</a>;
  return String(v);
}

function ResultTable({ r, id }: { r: Result; id: string }) {
  return (
    <div class="result">
      <table class="grid">
        <thead><tr>{r.columns.map(c => <th>{c}</th>)}</tr></thead>
        <tbody>{r.rows.map(row => <tr>{row.map((v, i) => <td>{cell(r.columns[i]!, v, id)}</td>)}</tr>)}</tbody>
      </table>
    </div>
  );
}
