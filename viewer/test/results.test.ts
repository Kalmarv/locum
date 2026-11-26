import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Capture } from "../server/capture";

const schema = readFileSync(resolve(import.meta.dir, "../../capture/src/db.rs"), "utf8")
  .match(/pub const SCHEMA: &str = r#"([\s\S]*?)"#;/)![1]!;

test("an output of a buffer's bytes says which, and one of all of it says none", () => {
  const dir = mkdtempSync(join(tmpdir(), "locum-viewer-"));
  const path = join(dir, "capture.db");
  try {
    const db = new Database(path);
    db.exec(schema);
    db.exec("INSERT INTO meta VALUES('api','d3d12'),('schema_version','7')");
    db.exec("INSERT INTO resources(id, kind, width, bytes) VALUES(4, 'buffer', 65536, 65536), (5, 'buffer', 256, 256)");
    const insert = db.prepare("INSERT INTO results VALUES(?,?,?,?,?,?,?)");
    insert.run(7, 4, "k4", 0, null, 512, 256);
    insert.run(7, 5, "k5", 0, null, null, null);
    insert.finalize();
    db.close();
    const capture = new Capture("t", path, dir, "t");
    const rows = capture.results(7);
    capture.close();
    expect(rows.map(r => [r.resource.id, r.range])).toEqual([[4, [512, 256]], [5, null]]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
