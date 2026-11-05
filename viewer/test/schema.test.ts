import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { SCHEMAS, schemaReadable } from "../server/capture";

// A capture's schema version is written from these constants; a bump the viewer has not been checked against fails
// here instead of refusing every new capture.
const db = readFileSync(resolve(import.meta.dir, "../../capture/src/db.rs"), "utf8");
const version = (name: string) => Number(db.match(new RegExp(`pub const ${name}: u32 = (\\d+);`))![1]);

test("the viewer reads the schema each API's captures are written in", () => {
  expect(SCHEMAS.d3d11![1]).toBe(version("SCHEMA_VERSION"));
  expect(SCHEMAS.d3d12![0]).toBe(version("D3D12_SCHEMA_OLDEST"));
  expect(SCHEMAS.d3d12![1]).toBe(version("D3D12_SCHEMA_VERSION"));
});

test("versions outside the range, other APIs and non-numbers are refused", () => {
  expect(schemaReadable("d3d12", "2")).toBe(true);
  expect(schemaReadable("d3d12", "6")).toBe(true);
  expect(schemaReadable("d3d12", "1")).toBe(false);
  expect(schemaReadable("d3d12", "7")).toBe(false);
  expect(schemaReadable("d3d11", "1")).toBe(true);
  expect(schemaReadable("d3d11", "2")).toBe(false);
  expect(schemaReadable("vulkan", "1")).toBe(false);
  expect(schemaReadable("d3d12", "5x")).toBe(false);
});
