import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Capture } from "../server/capture";

// The tables as locum writes them: the capture schema from db.rs, d12_draw_descriptors from d3d12.rs.
const source = (file: string) => readFileSync(resolve(import.meta.dir, "../../capture/src", file), "utf8");
const schema = source("db.rs").match(/pub const SCHEMA: &str = r#"([\s\S]*?)"#;/)![1]!;
const descriptors = source("d3d12.rs").match(/CREATE TABLE d12_draw_descriptors[\s\S]*?;\s*CREATE INDEX[^;]*;/)![0];

test("a D3D12 draw's descriptors come with their flags, addresses as hex and resources by shape", () => {
  const dir = mkdtempSync(join(tmpdir(), "locum-viewer-"));
  const path = join(dir, "capture.db");
  try {
    const db = new Database(path);
    db.exec(schema);
    db.exec(descriptors);
    db.exec("INSERT INTO meta VALUES('api','d3d12'),('schema_version','6')");
    db.exec("INSERT INTO resources(id, kind, width, bytes) VALUES(50, 'buffer', 4096, 4096)");
    const insert = db.prepare("INSERT INTO d12_draw_descriptors VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)");
    insert.run(1, "ps", "srv", 1, 0, 0, 9, 21, 121, null, null, null, null, null, null, "null");
    insert.run(1, "ps", "cb", 0, 0, 0, 9, 20, 120, 50, 0x123450010000, 512, null, null, null, null);
    insert.run(1, "vs", "srv", 0, 0, 1, null, null, null, 50, 0x123450000000, 4096, 16, 0, 1, null);
    insert.run(2, "ps", "srv", 0, 0, 0, 9, 22, null, null, null, null, null, null, null, "empty");
    insert.finalize();
    db.close();
    const capture = new Capture("t", path, dir, "t");
    const rows = capture.drawDescriptors(1)!;
    capture.close();
    expect(rows.map(r => [r.stage, r.kind, r.slot, r.flag])).toEqual([
      ["vs", "srv", 0, null], ["ps", "cb", 0, null], ["ps", "srv", 1, "null"],
    ]);
    expect(rows[1]!.address).toBe("0x123450010000");
    expect(rows[1]!.resource?.kind).toBe("buffer");
    expect(rows[2]!.resource).toBeNull();
    expect(rows[0]!.heap).toBeNull();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a capture without d12_draw_descriptors has none to show", () => {
  const dir = mkdtempSync(join(tmpdir(), "locum-viewer-"));
  const path = join(dir, "capture.db");
  try {
    const db = new Database(path);
    db.exec(schema);
    db.exec("INSERT INTO meta VALUES('api','d3d12'),('schema_version','6')");
    db.close();
    const capture = new Capture("t", path, dir, "t");
    expect(capture.drawDescriptors(1)).toBeNull();
    capture.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
