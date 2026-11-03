// Runs locum-cli for what it already does well: decoding textures to PNG, disassembling shaders, verifying.
// Outputs are cached under the viewer's cache folder, keyed by the capture and the arguments.

import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const viewerRoot = resolve(import.meta.dir, "..");
export const cacheRoot = join(viewerRoot, ".cache");

/** The locum-cli to run: --cli, LOCUM_CLI (viewer/.env sets it on this machine), PATH, or the repo's release build. */
export function findCli(given?: string): string | null {
  const candidates = [
    given,
    process.env.LOCUM_CLI,
    Bun.which("locum-cli"),
    resolve(viewerRoot, "../target/release/locum-cli.exe"),
    resolve(viewerRoot, "../target/release/locum-cli"),
  ];
  for (const c of candidates) if (c && existsSync(c)) return resolve(c);
  return null;
}

export class Cli {
  private running = 0;
  private queue: (() => void)[] = [];
  private inflight = new Map<string, Promise<string>>();

  constructor(public exe: string | null, private limit = 3) {}

  private async slot<T>(f: () => Promise<T>): Promise<T> {
    if (this.running >= this.limit) await new Promise<void>(r => this.queue.push(r));
    this.running++;
    try {
      return await f();
    } finally {
      this.running--;
      this.queue.shift()?.();
    }
  }

  /** Runs locum-cli with `args`: its output and exit code. */
  async runAll(args: string[], timeoutMs = 120_000): Promise<{ stdout: string; stderr: string; code: number }> {
    const exe = this.exe;
    if (!exe) throw new Error("locum-cli not found: pass --cli or set LOCUM_CLI");
    return this.slot(async () => {
      const p = Bun.spawn([exe, ...args], { stdout: "pipe", stderr: "pipe", stdin: "ignore", windowsHide: true });
      const timer = setTimeout(() => p.kill(), timeoutMs);
      const [stdout, stderr, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
      clearTimeout(timer);
      return { stdout, stderr, code };
    });
  }

  /** Runs locum-cli with `args`; resolves to standard output, rejects with standard error. */
  async run(args: string[], timeoutMs = 120_000): Promise<string> {
    const { stdout, stderr, code } = await this.runAll(args, timeoutMs);
    if (code !== 0) throw new Error((stderr || stdout).trim() || `locum-cli exited with ${code}`);
    return stdout;
  }

  /**
   * The file `path` (under the cache), made by `make(tmp)` the first time it is asked for. Concurrent requests for
   * the same file share one run; a failed run leaves nothing behind.
   */
  async cached(path: string, make: (tmp: string) => Promise<void>): Promise<string> {
    if (existsSync(path)) return path;
    let p = this.inflight.get(path);
    if (!p) {
      p = (async () => {
        mkdirSync(dirname(path), { recursive: true });
        const ext = path.slice(path.lastIndexOf("."));
        const tmp = `${path}.${process.pid}.${Date.now()}.part${ext}`;
        try {
          await make(tmp);
          renameSync(tmp, path);
          return path;
        } finally {
          rmSync(tmp, { force: true });
          this.inflight.delete(path);
        }
      })();
      this.inflight.set(path, p);
    }
    return p;
  }

  /** Text output of a cli command, cached in `path`. */
  async cachedText(path: string, args: string[], timeoutMs?: number): Promise<string> {
    await this.cached(path, async tmp => {
      await Bun.write(tmp, await this.run(args, timeoutMs));
    });
    return Bun.file(path).text();
  }
}
