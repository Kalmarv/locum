// A small headless-Chrome driver over the DevTools protocol, for checking the viewer in a real browser without a
// window. Its profile and screenshots go under viewer/.cache (screenshots of game captures are private).
//
//   const b = await Browser.launch();  const p = await b.page("http://localhost:5170/");
//   await p.eval("document.title");  await p.screenshot("x.png");  await b.close();

import { existsSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

const CHROMES = [
  process.env.CHROME,
  "C:/Program Files/Google/Chrome/Application/chrome.exe",
  "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
];

export const shots = resolve(import.meta.dir, "../.cache/shots");

export class Page {
  private next = 1;
  private waiting = new Map<number, { ok: (v: any) => void; fail: (e: Error) => void }>();
  readonly console: string[] = [];

  constructor(private ws: WebSocket) {
    ws.addEventListener("message", ev => {
      const m = JSON.parse(String(ev.data));
      if (m.id && this.waiting.has(m.id)) {
        const w = this.waiting.get(m.id)!;
        this.waiting.delete(m.id);
        m.error ? w.fail(new Error(m.error.message)) : w.ok(m.result);
      } else if (m.method === "Runtime.consoleAPICalled") {
        this.console.push(`${m.params.type}: ${m.params.args.map((a: any) => a.value ?? a.description).join(" ")}`);
      } else if (m.method === "Runtime.exceptionThrown") {
        this.console.push(`exception: ${m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text}`);
      }
    });
  }

  send(method: string, params: object = {}): Promise<any> {
    const id = this.next++;
    return new Promise((ok, fail) => {
      this.waiting.set(id, { ok, fail });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  async eval<T = any>(expression: string): Promise<T> {
    const r = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
    return r.result.value;
  }

  /** Waits until `expression` is truthy. */
  async until(expression: string, timeoutMs = 20000): Promise<any> {
    const end = Date.now() + timeoutMs;
    for (;;) {
      const v = await this.eval(expression).catch(() => null);
      if (v) return v;
      if (Date.now() > end) throw new Error(`timed out waiting for ${expression}`);
      await Bun.sleep(100);
    }
  }

  async goto(url: string) {
    await this.send("Page.navigate", { url });
    await this.until("document.readyState === 'complete'");
  }

  async key(key: string, code = key, text = key.length === 1 ? key : undefined) {
    const vk: Record<string, number> = { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38 };
    const base = { key, code, windowsVirtualKeyCode: vk[key] ?? key.toUpperCase().charCodeAt(0), text };
    await this.send("Input.dispatchKeyEvent", { type: "keyDown", ...base });
    await this.send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
  }

  async mouse(type: "mouseMoved" | "mousePressed" | "mouseReleased" | "mouseWheel", x: number, y: number, extra: object = {}) {
    await this.send("Input.dispatchMouseEvent", { type, x, y, button: type === "mouseMoved" || type === "mouseWheel" ? "none" : "left", clickCount: 1, ...extra });
  }

  async click(x: number, y: number) {
    await this.mouse("mouseMoved", x, y);
    await this.mouse("mousePressed", x, y);
    await this.mouse("mouseReleased", x, y);
  }

  /** The centre of the first element matching `selector` (optionally whose text includes `text`). */
  async center(selector: string, text?: string): Promise<{ x: number; y: number }> {
    const r = await this.eval(`(() => {
      const els = [...document.querySelectorAll(${JSON.stringify(selector)})];
      const el = els.find(e => ${JSON.stringify(text ?? "")} === "" || e.textContent.includes(${JSON.stringify(text ?? "")}));
      if (!el) return null;
      el.scrollIntoView({ block: "nearest" });
      const b = el.getBoundingClientRect();
      return { x: b.x + b.width / 2, y: b.y + b.height / 2 };
    })()`);
    if (!r) throw new Error(`no ${selector}${text ? ` with "${text}"` : ""}`);
    return r;
  }

  async clickOn(selector: string, text?: string) {
    const { x, y } = await this.center(selector, text);
    await this.click(x, y);
  }

  async screenshot(name: string): Promise<string> {
    mkdirSync(shots, { recursive: true });
    const r = await this.send("Page.captureScreenshot", { format: "png" });
    const path = join(shots, name);
    await Bun.write(path, Buffer.from(r.data, "base64"));
    return path;
  }
}

export class Browser {
  private constructor(private proc: Bun.Subprocess, private port: number) {}

  static async launch(width = 1600, height = 1000): Promise<Browser> {
    const exe = CHROMES.find(c => c && existsSync(c));
    if (!exe) throw new Error("no Chrome or Edge found (set CHROME)");
    const profile = resolve(import.meta.dir, "../.cache/chrome-profile");
    mkdirSync(profile, { recursive: true });
    const port = 9300 + Math.floor(Math.random() * 500);
    const proc = Bun.spawn([exe, "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
      `--window-size=${width},${height}`, "--no-first-run", "--no-default-browser-check", "--disable-extensions", "about:blank"],
      { stdout: "ignore", stderr: "ignore", windowsHide: true });
    const b = new Browser(proc, port);
    const end = Date.now() + 15000;
    for (;;) {
      try {
        await fetch(`http://127.0.0.1:${port}/json/version`);
        break;
      } catch {
        if (Date.now() > end) throw new Error("Chrome did not start");
        await Bun.sleep(200);
      }
    }
    return b;
  }

  async page(url: string): Promise<Page> {
    const targets = (await (await fetch(`http://127.0.0.1:${this.port}/json/list`)).json()) as any[];
    const t = targets.find(t => t.type === "page");
    const ws = new WebSocket(t.webSocketDebuggerUrl);
    await new Promise((ok, fail) => { ws.onopen = ok; ws.onerror = fail; });
    const p = new Page(ws);
    await p.send("Runtime.enable");
    await p.send("Page.enable");
    await p.goto(url);
    return p;
  }

  async close() {
    this.proc.kill();
    await this.proc.exited;
  }
}
