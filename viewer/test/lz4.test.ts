import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { lz4Block } from "../server/lz4";

// What the block in lz4-block.bin encodes: a slow ramp (long matches), noise (long literal runs) and "abc" repeated (a
// match overlapping what it writes). The block was written by lz4_flex through primase-foundations, as locum's blob
// store compresses a blob.
function input(): Uint8Array {
  const out = new Uint8Array(20_000 + 3000 + 3000);
  for (let i = 0; i < 20_000; i++) out[i] = (i / 64) & 255;
  let x = 12345;
  for (let i = 0; i < 3000; i++) {
    x = (Math.imul(x, 1_103_515_245) + 12345) >>> 0;
    out[20_000 + i] = x >>> 24;
  }
  for (let i = 0; i < 3000; i++) out[23_000 + i] = "abc".charCodeAt(i % 3);
  return out;
}

const block = new Uint8Array(readFileSync(resolve(import.meta.dir, "lz4-block.bin")));

test("an LZ4 block from locum's store decodes to the bytes it was made from", () => {
  expect(block.length).toBeLessThan(10_000);
  expect(lz4Block(block, 26_000)).toEqual(input());
});

test("a block is refused when its size is not the one it decodes to, or it is cut short", () => {
  expect(() => lz4Block(block, 25_999)).toThrow(/past the 25999 bytes it decodes to/);
  expect(() => lz4Block(block, 26_001)).toThrow(/decoded 26000 bytes, not 26001/);
  expect(() => lz4Block(block.subarray(0, block.length - 10), 26_000)).toThrow();
  // A match reaching before the output's start.
  expect(() => lz4Block(new Uint8Array([0x04, 0x01, 0x00]), 8)).toThrow(/before the output's start/);
});
