import { expect, test } from "bun:test";
import type { Row } from "../src/api";
import { summary } from "../src/pipeline";

const row = (kind: string, call: string, args: unknown): Row => ({
  seq: 1, kind, call, context: 0, deferred: 1, list: 1, draw: null, pass: null, group: 0,
  args: JSON.stringify(args), data: null, vs: null, hs: null, ds: null, gs: null, ps: null, cs: null,
});

test("D3D12 copies name their source and destination from positional arguments", () => {
  // Elden Ring's light-set copy: [dst, dst offset, src, src offset, bytes].
  expect(summary(row("copy", "CopyBufferRegion", [2878, 0, 1222, 10338304, 50400])))
    .toBe("#1222 → #2878 50400 bytes, offset 10338304 → 0");
  expect(summary(row("copy", "CopyResource", [5, 6]))).toBe("#6 → #5");
  expect(summary(row("copy", "CopyTextureRegion", [{ resource: 7, sub: 0 }, 0, 0, 0, { resource: 8, sub: 1 }, null])))
    .toBe("#8 → #7");
});

test("D3D12 clears name the view and its resource", () => {
  const view = { definition: { id: 85736004, resource: 38051 }, heap: 1208, slot: 770 };
  expect(summary(row("clear", "ClearRenderTargetView", [view, [0, 0, 0, 1], []]))).toContain("view #85736004 of #38051");
});

test("D3D11 events keep their named arguments", () => {
  expect(summary(row("copy", "CopyResource", { src: 3, dst: 4 }))).toBe("#3 → #4");
});
