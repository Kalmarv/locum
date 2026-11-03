// What a capture holds and what it lacks (as `locum-cli info` says it), its meta, and verify.

import { useState } from "preact/hooks";
import { get, post, type Info } from "./api";
import { KV, Section, useCtx } from "./parts";
import { mib, useAsync } from "./util";

export function InfoPage({ info }: { info: Info }) {
  const m = info.meta;
  return (
    <div class="page">
      <h2>capture {info.label}</h2>
      <KV rows={[
        ["database", <code>{info.path}</code>],
        ["frame", `${m.frame ?? "?"} of ${m.exe ?? "?"} (pid ${m.pid ?? "?"}), ${m.api ?? "?"}, locum ${m.locum ?? "?"}${m.time ? `, ${new Date(Number(m.time) * 1000).toLocaleString()}` : ""}`],
        ["request", <code>{JSON.stringify(m.request)}</code>],
        ["blob store", <span><code>{info.blobs}</code>{info.storeExists ? "" : <span class="warn"> does not exist</span>}</span>],
      ]} />
      <div class="stats">
        <Stat label="events" value={info.timeline.events} sub={info.timeline.kinds.map(([k, n]) => `${k} ${n}`).join(", ")} />
        <Stat label="draws and dispatches" value={info.timeline.draws} sub={`in ${info.timeline.passes} passes`} />
        <Stat label="command lists spliced" value={info.lists.spliced} sub={`${info.lists.recordedBefore} recorded the frame before, ${info.lists.unrecorded} executed without a recording`} />
        <Stat label="constant buffer contents" value={info.constants.distinct} sub={`${info.constants.failures} copies failed`} />
        <Stat label="resources live" value={info.resources.live} sub={`${info.resources.used} used (${info.resources.written} written); contents for ${info.resources.contents} (${mib(info.resources.contentBytes)}): ${info.resources.sources.map(([s, n]) => `${s} ${n}`).join(", ")}`} />
        <Stat label="uploads" value={info.uploads.total} sub={`${info.uploads.stored} stored, ${info.uploads.hashOnly} hash only; ${info.uploads.none} have no upload hash`} />
        <Stat label="targets read back" value={info.targets.count} sub={`${mib(info.targets.bytes)}, ${info.targets.skipped} skipped`} />
        <Stat label="shaders" value={info.shaders.count} sub={`${info.shaders.withBytecode} with bytecode`} />
        <Stat label="blobs referenced" value={info.blobsReferenced} sub={`${info.blobsAbsent} not in the store`} />
      </div>
      {info.memory ? <KV rows={[["memory dump", info.memory]]} /> : null}
      <h3>missing</h3>
      {info.missing.length ? <ul class="missing">{info.missing.map(x => <li>{x}</li>)}</ul> : <div class="ok">nothing</div>}
      <Verify />
      <Section title="meta" open={false}>
        <table class="kv"><tbody>{Object.entries(m).map(([k, v]) => <tr><th>{k}</th><td><code class="wrap">{typeof v === "string" ? v : JSON.stringify(v)}</code></td></tr>)}</tbody></table>
      </Section>
    </div>
  );
}

function Stat({ label, value, sub }: { label: string; value: number; sub: string }) {
  return <div class="stat"><div class="stat-value">{value.toLocaleString()}</div><div class="stat-label">{label}</div><div class="stat-sub">{sub}</div></div>;
}

/** `locum-cli verify`: replays the frame and compares every target read back with the game's. */
function Verify() {
  const { id } = useCtx();
  const [run, setRun] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const prior = useAsync(() => get<{ text: string | null }>(`c/${id}/verify`), [id, run]);
  async function start() {
    setBusy(true);
    setError(null);
    try {
      await post(`c/${id}/verify`, {});
    } catch (e) {
      setError(String((e as Error).message ?? e));
    } finally {
      setBusy(false);
      setRun(run + 1);
    }
  }
  return (
    <Section title="verify (replay the frame and compare every target)" open={!!prior.data?.text}>
      {prior.data?.text ? <pre class="verify">{prior.data.text}</pre> : (
        <div>
          <button onClick={start} disabled={busy}>{busy ? "replaying… (this runs the whole frame on the GPU)" : "run locum-cli verify"}</button>
          {error ? <pre class="error">{error}</pre> : null}
        </div>
      )}
    </Section>
  );
}
