// A minimal PNG writer (8-bit RGBA, no filtering) for pictures the viewer makes itself, like target diffs, and a
// reader for the 8-bit pictures locum-cli writes, so a picture can be shown one channel at a time or without alpha.

import { deflateSync, inflateSync } from "node:zlib";

const CRC = new Uint32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(parts: Uint8Array[]): number {
  let c = 0xffffffff;
  for (const p of parts) for (let i = 0; i < p.length; i++) c = CRC[(c ^ p[i]!) & 0xff]! ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type: string, data: Uint8Array): Uint8Array {
  const out = new Uint8Array(12 + data.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, data.length);
  const t = new TextEncoder().encode(type);
  out.set(t, 4);
  out.set(data, 8);
  v.setUint32(8 + data.length, crc32([t, data]));
  return out;
}

export function encodePng(width: number, height: number, rgba: Uint8Array): Uint8Array<ArrayBuffer> {
  const raw = new Uint8Array((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) raw.set(rgba.subarray(y * width * 4, (y + 1) * width * 4), y * (width * 4 + 1) + 1);
  const ihdr = new Uint8Array(13);
  const v = new DataView(ihdr.buffer);
  v.setUint32(0, width);
  v.setUint32(4, height);
  ihdr.set([8, 6, 0, 0, 0], 8);
  const parts = [
    new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", new Uint8Array(deflateSync(raw, { level: 3 }))),
    chunk("IEND", new Uint8Array()),
  ];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
}

/** The pixels of an 8-bit, non-interlaced greyscale, grey+alpha, RGB or RGBA PNG, as RGBA. */
export function decodePng(png: Uint8Array): { width: number; height: number; rgba: Uint8Array } {
  const v = new DataView(png.buffer, png.byteOffset, png.byteLength);
  if (v.getUint32(0) !== 0x89504e47) throw new Error("not a PNG");
  let width = 0, height = 0, type = 0;
  const idat: Uint8Array[] = [];
  for (let at = 8; at + 8 <= png.length;) {
    const len = v.getUint32(at);
    const t = String.fromCharCode(...png.subarray(at + 4, at + 8));
    const data = png.subarray(at + 8, at + 8 + len);
    if (t === "IHDR") {
      width = v.getUint32(at + 8);
      height = v.getUint32(at + 12);
      type = data[9]!;
      if (data[8] !== 8 || data[12] !== 0) throw new Error(`PNG with bit depth ${data[8]}, interlace ${data[12]}: not read`);
    } else if (t === "IDAT") idat.push(data);
    else if (t === "IEND") break;
    at += 12 + len;
  }
  const bpp = ({ 0: 1, 2: 3, 4: 2, 6: 4 } as Record<number, number>)[type];
  if (!bpp) throw new Error(`PNG colour type ${type}: not read`);
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * bpp;
  const px = new Uint8Array(stride * height);
  for (let y = 0; y < height; y++) {
    const f = raw[y * (stride + 1)]!;
    const row = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const o = y * stride;
    for (let i = 0; i < stride; i++) {
      const a = i >= bpp ? px[o + i - bpp]! : 0, b = y ? px[o - stride + i]! : 0, c = y && i >= bpp ? px[o - stride + i - bpp]! : 0;
      let p = row[i]!;
      if (f === 1) p += a;
      else if (f === 2) p += b;
      else if (f === 3) p += (a + b) >> 1;
      else if (f === 4) {
        const q = a + b - c, pa = Math.abs(q - a), pb = Math.abs(q - b), pc = Math.abs(q - c);
        p += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      px[o + i] = p;
    }
  }
  const rgba = new Uint8Array(width * height * 4);
  for (let i = 0, j = 0; i < width * height; i++, j += bpp) {
    const g = px[j]!;
    if (bpp === 1) rgba.set([g, g, g, 255], i * 4);
    else if (bpp === 2) rgba.set([g, g, g, px[j + 1]!], i * 4);
    else if (bpp === 3) rgba.set([g, px[j + 1]!, px[j + 2]!, 255], i * 4);
    else rgba.set(px.subarray(j, j + 4), i * 4);
  }
  return { width, height, rgba };
}

export const CHANNELS = ["rgb", "rgba", "r", "g", "b", "a"] as const;
export type Channels = (typeof CHANNELS)[number];

/**
 * Picks what to show of an RGBA picture: rgb drops alpha (a target's alpha is rarely coverage, and a 0 there would
 * hide the colour), one channel shows it as grey, rgba is the picture as it is.
 */
export function selectChannels(rgba: Uint8Array, channels: Channels): Uint8Array {
  if (channels === "rgba") return rgba;
  const out = new Uint8Array(rgba.length);
  const c = "rgba".indexOf(channels);
  for (let i = 0; i < rgba.length; i += 4) {
    if (channels === "rgb") out.set(rgba.subarray(i, i + 3), i);
    else out[i] = out[i + 1] = out[i + 2] = rgba[i + c]!;
    out[i + 3] = 255;
  }
  return out;
}
