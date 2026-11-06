// DXGI formats: names, block layout, and reading one texel's raw value. Mirrors capture/src/format.rs and
// capture/src/texels.rs; images themselves are decoded by locum-cli, this only reads the texel under the picker.

const NAMES: Record<number, string> = {
  0: "UNKNOWN", 1: "R32G32B32A32_TYPELESS", 2: "R32G32B32A32_FLOAT", 3: "R32G32B32A32_UINT", 4: "R32G32B32A32_SINT",
  5: "R32G32B32_TYPELESS", 6: "R32G32B32_FLOAT", 7: "R32G32B32_UINT", 8: "R32G32B32_SINT",
  9: "R16G16B16A16_TYPELESS", 10: "R16G16B16A16_FLOAT", 11: "R16G16B16A16_UNORM", 12: "R16G16B16A16_UINT",
  13: "R16G16B16A16_SNORM", 14: "R16G16B16A16_SINT", 15: "R32G32_TYPELESS", 16: "R32G32_FLOAT", 17: "R32G32_UINT",
  18: "R32G32_SINT", 19: "R32G8X24_TYPELESS", 20: "D32_FLOAT_S8X24_UINT", 21: "R32_FLOAT_X8X24_TYPELESS",
  22: "X32_TYPELESS_G8X24_UINT", 23: "R10G10B10A2_TYPELESS", 24: "R10G10B10A2_UNORM", 25: "R10G10B10A2_UINT",
  26: "R11G11B10_FLOAT", 27: "R8G8B8A8_TYPELESS", 28: "R8G8B8A8_UNORM", 29: "R8G8B8A8_UNORM_SRGB",
  30: "R8G8B8A8_UINT", 31: "R8G8B8A8_SNORM", 32: "R8G8B8A8_SINT", 33: "R16G16_TYPELESS", 34: "R16G16_FLOAT",
  35: "R16G16_UNORM", 36: "R16G16_UINT", 37: "R16G16_SNORM", 38: "R16G16_SINT", 39: "R32_TYPELESS", 40: "D32_FLOAT",
  41: "R32_FLOAT", 42: "R32_UINT", 43: "R32_SINT", 44: "R24G8_TYPELESS", 45: "D24_UNORM_S8_UINT",
  46: "R24_UNORM_X8_TYPELESS", 47: "X24_TYPELESS_G8_UINT", 48: "R8G8_TYPELESS", 49: "R8G8_UNORM", 50: "R8G8_UINT",
  51: "R8G8_SNORM", 52: "R8G8_SINT", 53: "R16_TYPELESS", 54: "R16_FLOAT", 55: "D16_UNORM", 56: "R16_UNORM",
  57: "R16_UINT", 58: "R16_SNORM", 59: "R16_SINT", 60: "R8_TYPELESS", 61: "R8_UNORM", 62: "R8_UINT", 63: "R8_SNORM",
  64: "R8_SINT", 65: "A8_UNORM", 66: "R1_UNORM", 67: "R9G9B9E5_SHAREDEXP", 68: "R8G8_B8G8_UNORM",
  69: "G8R8_G8B8_UNORM", 70: "BC1_TYPELESS", 71: "BC1_UNORM", 72: "BC1_UNORM_SRGB", 73: "BC2_TYPELESS",
  74: "BC2_UNORM", 75: "BC2_UNORM_SRGB", 76: "BC3_TYPELESS", 77: "BC3_UNORM", 78: "BC3_UNORM_SRGB",
  79: "BC4_TYPELESS", 80: "BC4_UNORM", 81: "BC4_SNORM", 82: "BC5_TYPELESS", 83: "BC5_UNORM", 84: "BC5_SNORM",
  85: "B5G6R5_UNORM", 86: "B5G5R5A1_UNORM", 87: "B8G8R8A8_UNORM", 88: "B8G8R8X8_UNORM",
  89: "R10G10B10_XR_BIAS_A2_UNORM", 90: "B8G8R8A8_TYPELESS", 91: "B8G8R8A8_UNORM_SRGB", 92: "B8G8R8X8_TYPELESS",
  93: "B8G8R8X8_UNORM_SRGB", 94: "BC6H_TYPELESS", 95: "BC6H_UF16", 96: "BC6H_SF16", 97: "BC7_TYPELESS",
  98: "BC7_UNORM", 99: "BC7_UNORM_SRGB", 100: "AYUV", 101: "Y410", 102: "Y416", 103: "NV12", 104: "P010",
  105: "P016", 106: "420_OPAQUE", 107: "YUY2", 108: "Y210", 109: "Y216", 110: "NV11", 111: "AI44", 112: "IA44",
  113: "P8", 114: "A8P8", 115: "B4G4R4A4_UNORM", 130: "P208", 131: "V208", 132: "V408", 189: "A4B4G4R4_UNORM",
  190: "A4R4G4B4_UNORM",
};

export function formatName(format: number | null | undefined): string {
  if (format == null) return "-";
  return NAMES[format] ?? `UNKNOWN_FORMAT(${format})`;
}

export function formatNames(): Record<number, string> {
  return NAMES;
}

export interface Layout {
  blockWidth: number;
  blockHeight: number;
  blockBytes: number;
}

const plainLayout = (bytes: number): Layout => ({ blockWidth: 1, blockHeight: 1, blockBytes: bytes });
const blocks = (w: number, h: number, bytes: number): Layout => ({ blockWidth: w, blockHeight: h, blockBytes: bytes });

/** The layout of a format (capture/src/format.rs `layout`), or null when it is not one plane of blocks. */
export function layout(f: number): Layout | null {
  if (f >= 1 && f <= 4) return plainLayout(16);
  if (f >= 5 && f <= 8) return plainLayout(12);
  if (f >= 9 && f <= 22) return plainLayout(8);
  if (f >= 23 && f <= 47) return plainLayout(4);
  if (f >= 48 && f <= 59) return plainLayout(2);
  if (f >= 60 && f <= 65) return plainLayout(1);
  if (f === 66) return blocks(8, 1, 1);
  if (f === 67) return plainLayout(4);
  if (f === 68 || f === 69) return blocks(2, 1, 4);
  if ((f >= 70 && f <= 72) || (f >= 79 && f <= 81)) return blocks(4, 4, 8);
  if ((f >= 73 && f <= 78) || (f >= 82 && f <= 84) || (f >= 94 && f <= 99)) return blocks(4, 4, 16);
  if ([85, 86, 115, 189, 190].includes(f)) return plainLayout(2);
  if ((f >= 87 && f <= 93) || f === 100 || f === 101) return plainLayout(4);
  if (f === 102) return plainLayout(8);
  if (f === 107) return blocks(2, 1, 4);
  if (f === 108 || f === 109) return blocks(2, 1, 8);
  if (f >= 111 && f <= 113) return plainLayout(1);
  if (f === 114) return plainLayout(2);
  return null;
}

export function isCompressed(f: number): boolean {
  return (f >= 70 && f <= 84) || (f >= 94 && f <= 99);
}

/** Typeless and depth formats as the plain format of the same bits (capture/src/format.rs `readable`). */
export function readable(f: number): number {
  const map: Record<number, number> = {
    1: 2, 5: 6, 9: 10, 15: 16, 19: 21, 20: 21, 23: 24, 27: 28, 33: 34, 39: 41, 40: 41, 44: 46, 45: 46, 48: 49,
    53: 56, 55: 56, 60: 61, 70: 71, 73: 74, 76: 77, 79: 80, 82: 83, 90: 87, 92: 88, 94: 95, 97: 98,
  };
  return map[f] ?? f;
}

/** One subresource in the canonical layout (capture/src/layout.rs `Sub`). */
export interface Sub {
  index: number;
  mip: number;
  slice: number;
  width: number;
  height: number;
  depth: number;
  rowBytes: number;
  rows: number;
  offset: number;
  size: number;
}

/**
 * A texture's subresources in the canonical layout. A multisampled texture's (one mip) holds its samples' images one
 * after another: each is given as one image of them stacked top to bottom, as its targets are read back.
 */
export function textureSubs(format: number, width: number, height: number, depth: number, mips: number,
  array: number, samples = 1): Sub[] | null {
  const l = layout(format);
  if (!l) return null;
  const out: Sub[] = [];
  let offset = 0;
  mips = Math.max(mips, 1);
  const stacked = Math.max(samples, 1);
  for (let slice = 0; slice < Math.max(array, 1); slice++) {
    for (let mip = 0; mip < mips; mip++) {
      const w = Math.max(width >> mip, 1), h = Math.max(height >> mip, 1) * stacked, d = Math.max(depth >> mip, 1);
      const rowBytes = Math.ceil(w / l.blockWidth) * l.blockBytes, rows = Math.ceil(h / l.blockHeight);
      const size = rowBytes * rows * d;
      out.push({ index: mip + slice * mips, mip, slice, width: w, height: h, depth: d, rowBytes, rows, offset, size });
      offset += size;
    }
  }
  return out;
}

export function f16(bits: number): number {
  const sign = bits & 0x8000 ? -1 : 1;
  const exp = (bits >> 10) & 31, mant = bits & 0x3ff;
  if (exp === 0) return sign * mant * 2 ** -24;
  if (exp === 31) return mant === 0 ? sign * Infinity : NaN;
  return sign * (1 + mant / 1024) * 2 ** (exp - 15);
}

function smallFloat(bits: number, mantissa: number): number {
  const exp = (bits >>> mantissa) & 31;
  const mant = (bits & ((1 << mantissa) - 1)) / (1 << mantissa);
  if (exp === 0) return mant * 2 ** -14;
  if (exp === 31) return mant === 0 ? Infinity : NaN;
  return (1 + mant) * 2 ** (exp - 15);
}

type Kind = "float" | "unorm" | "snorm" | "uint" | "sint";

function plain(f: number): [number, number, Kind] | null {
  const t: Record<number, [number, number, Kind]> = {
    2: [4, 32, "float"], 3: [4, 32, "uint"], 4: [4, 32, "sint"], 6: [3, 32, "float"], 7: [3, 32, "uint"],
    8: [3, 32, "sint"], 10: [4, 16, "float"], 11: [4, 16, "unorm"], 12: [4, 16, "uint"], 13: [4, 16, "snorm"],
    14: [4, 16, "sint"], 16: [2, 32, "float"], 17: [2, 32, "uint"], 18: [2, 32, "sint"], 28: [4, 8, "unorm"],
    29: [4, 8, "unorm"], 30: [4, 8, "uint"], 31: [4, 8, "snorm"], 32: [4, 8, "sint"], 34: [2, 16, "float"],
    35: [2, 16, "unorm"], 36: [2, 16, "uint"], 37: [2, 16, "snorm"], 38: [2, 16, "sint"], 41: [1, 32, "float"],
    42: [1, 32, "uint"], 43: [1, 32, "sint"], 49: [2, 8, "unorm"], 50: [2, 8, "uint"], 51: [2, 8, "snorm"],
    52: [2, 8, "sint"], 54: [1, 16, "float"], 56: [1, 16, "unorm"], 57: [1, 16, "uint"], 58: [1, 16, "snorm"],
    59: [1, 16, "sint"], 61: [1, 8, "unorm"], 62: [1, 8, "uint"], 63: [1, 8, "snorm"], 64: [1, 8, "sint"],
  };
  return t[f] ?? null;
}

function channel(v: DataView, at: number, bits: number, kind: Kind): number {
  if (bits === 32) {
    if (kind === "float") return v.getFloat32(at, true);
    if (kind === "uint") return v.getUint32(at, true);
    return v.getInt32(at, true);
  }
  if (bits === 16) {
    const u = v.getUint16(at, true);
    switch (kind) {
      case "float": return f16(u);
      case "unorm": return u / 65535;
      case "snorm": return Math.max(v.getInt16(at, true) / 32767, -1);
      case "uint": return u;
      case "sint": return v.getInt16(at, true);
    }
  }
  const u = v.getUint8(at);
  switch (kind) {
    case "unorm": case "float": return u / 255;
    case "snorm": return Math.max(v.getInt8(at) / 127, -1);
    case "uint": return u;
    case "sint": return v.getInt8(at);
  }
}

export interface Texel {
  /** The channel names and their values as the format reads them. */
  channels: [string, number][];
  /** The texel's (or for block-compressed formats, its block's) bytes, hex. */
  raw: string;
  /** Integer formats hold integers; the rest are normalized or float. */
  kind: string;
  note?: string;
}

const RGBA = ["r", "g", "b", "a"];

/**
 * Reads the texel at (x, y) of a subresource whose bytes are `bytes` (canonical layout: tight rows of blocks), in
 * `format`. Block-compressed formats return the block's bytes only; their decoded value comes from the picture.
 */
export function readTexel(format: number, width: number, bytes: Uint8Array, x: number, y: number): Texel | null {
  const f = readable(format);
  const l = layout(f);
  if (!l) return null;
  const rowBytes = Math.ceil(width / l.blockWidth) * l.blockBytes;
  const at = Math.floor(y / l.blockHeight) * rowBytes + Math.floor(x / l.blockWidth) * l.blockBytes;
  if (at + l.blockBytes > bytes.length) return null;
  const raw = [...bytes.subarray(at, at + l.blockBytes)].map(b => b.toString(16).padStart(2, "0")).join(" ");
  const v = new DataView(bytes.buffer, bytes.byteOffset + at, l.blockBytes);
  if (isCompressed(f)) return { channels: [], raw, kind: "block", note: `${l.blockWidth}x${l.blockHeight} block` };
  const p = plain(f);
  if (p) {
    const [n, bits, kind] = p;
    const channels: [string, number][] = [];
    for (let c = 0; c < n; c++) channels.push([RGBA[c]!, channel(v, (c * bits) / 8, bits, kind)]);
    return { channels, raw, kind };
  }
  const word = l.blockBytes >= 4 ? v.getUint32(0, true) : 0;
  const half = l.blockBytes >= 2 ? v.getUint16(0, true) : 0;
  switch (f) {
    case 24: case 25: {
      const c = [word & 1023, (word >>> 10) & 1023, (word >>> 20) & 1023, word >>> 30];
      const scale = f === 24 ? [1023, 1023, 1023, 3] : [1, 1, 1, 1];
      return { channels: c.map((x, i) => [RGBA[i]!, x / scale[i]!]), raw, kind: f === 24 ? "unorm" : "uint" };
    }
    case 26:
      return {
        channels: [["r", smallFloat(word & 0x7ff, 6)], ["g", smallFloat((word >>> 11) & 0x7ff, 6)],
          ["b", smallFloat(word >>> 22, 5)]], raw, kind: "float",
      };
    case 21: case 22:
      return {
        channels: [["depth", v.getFloat32(0, true)], ["stencil", v.getUint8(4)]], raw, kind: "float",
      };
    case 46: case 47:
      return { channels: [["depth", (word & 0xffffff) / 16777215], ["stencil", word >>> 24]], raw, kind: "unorm" };
    case 65:
      return { channels: [["a", v.getUint8(0) / 255]], raw, kind: "unorm" };
    case 67: {
      const scale = 2 ** ((word >>> 27) - 15 - 9);
      return {
        channels: [["r", (word & 511) * scale], ["g", ((word >>> 9) & 511) * scale], ["b", ((word >>> 18) & 511) * scale]],
        raw, kind: "float",
      };
    }
    case 85:
      return { channels: [["r", (half >> 11) / 31], ["g", ((half >> 5) & 63) / 63], ["b", (half & 31) / 31]], raw, kind: "unorm" };
    case 86:
      return {
        channels: [["r", ((half >> 10) & 31) / 31], ["g", ((half >> 5) & 31) / 31], ["b", (half & 31) / 31], ["a", half >> 15]],
        raw, kind: "unorm",
      };
    case 115:
      return { channels: [8, 4, 0, 12].map((s, i) => [RGBA[i]!, ((half >> s) & 15) / 15]), raw, kind: "unorm" };
    case 87: case 88: case 91: case 93: {
      const [b, g, r, a] = [0, 1, 2, 3].map(k => v.getUint8(k) / 255);
      const ch: [string, number][] = [["r", r!], ["g", g!], ["b", b!]];
      if (f === 87 || f === 91) ch.push(["a", a!]);
      return { channels: ch, raw, kind: "unorm" };
    }
  }
  return { channels: [], raw, kind: "raw", note: `${formatName(f)} is shown as bytes` };
}
