import { expect, test } from "bun:test";
import { deflateSync } from "node:zlib";
import { decodePng, encodePng, selectChannels } from "../server/png";

test("encode then decode gives the same pixels", () => {
  const rgba = new Uint8Array(5 * 3 * 4).map((_, i) => (i * 37) & 255);
  const p = decodePng(encodePng(5, 3, rgba));
  expect([p.width, p.height]).toEqual([5, 3]);
  expect(p.rgba).toEqual(rgba);
});

test("every row filter is undone", () => {
  // One 2x5 RGB picture, each row written with a different filter (0 none, 1 sub, 2 up, 3 average, 4 paeth).
  const w = 2, bpp = 3, stride = w * bpp;
  const px = [...Array(5)].map((_, y) => [...Array(stride)].map((_, i) => (y * 50 + i * 23) & 255));
  const paeth = (a: number, b: number, c: number) => {
    const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
  };
  const raw: number[] = [];
  px.forEach((row, y) => {
    raw.push(y);
    row.forEach((v, i) => {
      const a = i >= bpp ? row[i - bpp]! : 0, b = y ? px[y - 1]![i]! : 0, c = y && i >= bpp ? px[y - 1]![i - bpp]! : 0;
      const pred = [0, a, b, (a + b) >> 1, paeth(a, b, c)][y]!;
      raw.push((v - pred) & 255);
    });
  });
  const png = encodePng(w, 5, new Uint8Array(w * 5 * 4));
  // Swap in an RGB header and the filtered data.
  const ihdrType = 8 + 8 + 9;
  png[ihdrType] = 2;
  const head = png.subarray(0, 8 + 25);
  const p = decodePng(withIdat(head, new Uint8Array(deflateSync(new Uint8Array(raw)))));
  for (let y = 0; y < 5; y++)
    for (let x = 0; x < w; x++)
      expect([...p.rgba.subarray((y * w + x) * 4, (y * w + x) * 4 + 4)]).toEqual([...px[y]!.slice(x * 3, x * 3 + 3), 255]);
});

/** The reader does not check CRCs, so a header and an IDAT can be put together by hand. */
function withIdat(head: Uint8Array, idat: Uint8Array): Uint8Array {
  const out = new Uint8Array(head.length + 12 + idat.length + 12);
  out.set(head);
  const v = new DataView(out.buffer);
  v.setUint32(head.length, idat.length);
  out.set(new TextEncoder().encode("IDAT"), head.length + 4);
  out.set(idat, head.length + 8);
  const end = head.length + 12 + idat.length;
  out.set(new TextEncoder().encode("IEND"), end + 4);
  return out;
}

test("channels: rgb drops alpha, one channel is grey, rgba is as it was", () => {
  const px = new Uint8Array([10, 20, 30, 0, 200, 100, 50, 128]);
  expect([...selectChannels(px, "rgb")]).toEqual([10, 20, 30, 255, 200, 100, 50, 255]);
  expect([...selectChannels(px, "g")]).toEqual([20, 20, 20, 255, 100, 100, 100, 255]);
  expect([...selectChannels(px, "a")]).toEqual([0, 0, 0, 255, 128, 128, 128, 255]);
  expect(selectChannels(px, "rgba")).toBe(px);
});
