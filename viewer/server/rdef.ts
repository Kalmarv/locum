// Shader reflection from a DXBC container's RDEF chunk: each constant buffer's name, register and variables, so a
// constant shows as `cbTexScroll.uvScale.x`. A port of capture/src/rdef.rs.

export interface Type {
  /** D3D_SHADER_VARIABLE_CLASS: 0 scalar, 1 vector, 2 row-major matrix, 3 column-major matrix, 4 object, 5 struct. */
  class: number;
  /** D3D_SHADER_VARIABLE_TYPE: 1 bool, 2 int, 3 float, 19 uint, ... */
  base: number;
  rows: number;
  columns: number;
  /** Array length, 0 when not an array. */
  elements: number;
  members: { name: string; offset: number; ty: Type }[];
}

export interface Variable {
  name: string;
  offset: number;
  size: number;
  /** Whether the shader reads it. */
  used: boolean;
  ty: Type;
  type: string;
}

export interface ConstantBuffer {
  name: string;
  /** The register (`b<slot>`), when the shader binds it. */
  slot: number | null;
  space: number;
  size: number;
  variables: Variable[];
}

/** A DXBC container's chunk with the four-character code `code`. */
export function chunk(dxbc: Uint8Array, code: string): Uint8Array | null {
  const v = new DataView(dxbc.buffer, dxbc.byteOffset, dxbc.byteLength);
  if (dxbc.length < 32 || text(dxbc, 0, 4) !== "DXBC") return null;
  const count = v.getUint32(28, true);
  for (let i = 0; i < count; i++) {
    const at = 32 + i * 4;
    if (at + 4 > dxbc.length) return null;
    const offset = v.getUint32(at, true);
    if (offset + 8 > dxbc.length) return null;
    if (text(dxbc, offset, 4) === code) {
      const size = v.getUint32(offset + 4, true);
      return dxbc.subarray(offset + 8, Math.min(offset + 8 + size, dxbc.length));
    }
  }
  return null;
}

function text(b: Uint8Array, at: number, n: number): string {
  return String.fromCharCode(...b.subarray(at, at + n));
}

class Reader {
  v: DataView;
  constructor(public b: Uint8Array) {
    this.v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  }
  u32(at: number): number {
    if (at + 4 > this.b.length) throw new RangeError("RDEF out of range");
    return this.v.getUint32(at, true);
  }
  u16(at: number): number {
    if (at + 2 > this.b.length) throw new RangeError("RDEF out of range");
    return this.v.getUint16(at, true);
  }
  str(at: number): string {
    const end = this.b.indexOf(0, at);
    if (at >= this.b.length || end < 0) throw new RangeError("RDEF string out of range");
    return new TextDecoder().decode(this.b.subarray(at, end));
  }
}

/** HLSL spelling, like `float4`, `float4x4` or `uint`. */
export function typeName(t: Type): string {
  const bases: Record<number, string> = {
    0: "void", 1: "bool", 2: "int", 3: "float", 19: "uint", 20: "uint8", 39: "double", 54: "min8float",
    55: "min10float", 56: "min16float", 57: "min12int", 58: "min16int", 59: "min16uint",
  };
  const base = bases[t.base] ?? "?";
  let shape = base;
  if (t.class === 1) shape = `${base}${t.columns}`;
  else if (t.class === 2 || t.class === 3) shape = `${base}${t.rows}x${t.columns}`;
  else if (t.class === 5) shape = "struct";
  return t.elements > 0 ? `${shape}[${t.elements}]` : shape;
}

function readType(r: Reader, at: number, depth: number): Type {
  const t: Type = {
    class: r.u16(at), base: r.u16(at + 2), rows: r.u16(at + 4), columns: r.u16(at + 6), elements: r.u16(at + 8),
    members: [],
  };
  const members = r.u16(at + 10), membersAt = r.u32(at + 12);
  if (depth < 8) {
    for (let i = 0; i < members; i++) {
      const m = membersAt + i * 12;
      t.members.push({ name: r.str(r.u32(m)), ty: readType(r, r.u32(m + 4), depth + 1), offset: r.u32(m + 8) });
    }
  }
  return t;
}

/** The constant buffers a shader declares, from its DXBC bytecode, or null without reflection. */
export function constantBuffers(dxbc: Uint8Array): ConstantBuffer[] | null {
  const rdef = chunk(dxbc, "RDEF");
  if (!rdef) return null;
  try {
    const r = new Reader(rdef);
    const cbCount = r.u32(0), cbAt = r.u32(4), bindCount = r.u32(8), bindAt = r.u32(12);
    const major = rdef[17] ?? 0;
    // Shader model 5 adds an "RD11" header giving the record sizes; before it, variables are 24 bytes and bindings 32.
    const rd11 = major >= 5 && text(rdef, 28, 4) === "RD11";
    const [cbSize, bindSize, varSize] = rd11 ? [r.u32(36), r.u32(40), r.u32(44)] : [24, 32, 24];
    const registers: [string, number, number][] = [];
    for (let i = 0; i < bindCount; i++) {
      const b = bindAt + i * bindSize;
      const name = r.str(r.u32(b));
      if (r.u32(b + 4) === 0) registers.push([name, r.u32(b + 20), bindSize >= 40 ? r.u32(b + 32) : 0]);
    }
    const out: ConstantBuffer[] = [];
    for (let i = 0; i < cbCount; i++) {
      const c = cbAt + i * cbSize;
      const name = r.str(r.u32(c));
      const varCount = r.u32(c + 4), varAt = r.u32(c + 8), size = r.u32(c + 12);
      const variables: Variable[] = [];
      for (let k = 0; k < varCount; k++) {
        const at = varAt + k * varSize;
        const ty = readType(r, r.u32(at + 16), 0);
        variables.push({
          name: r.str(r.u32(at)), offset: r.u32(at + 4), size: r.u32(at + 8), used: (r.u32(at + 12) & 2) !== 0, ty,
          type: typeName(ty),
        });
      }
      const reg = registers.find(([n]) => n === name);
      out.push({ name, slot: reg ? reg[1] : null, space: reg ? reg[2] : 0, size, variables });
    }
    return out;
  } catch {
    return null;
  }
}

const COMPONENTS = ["x", "y", "z", "w"];

/** The bytes one array element of `ty` takes in a constant buffer (every element but the last starts a register). */
function elementStride(ty: Type): number {
  let bytes: number;
  if (ty.class === 5) {
    bytes = ty.members.length
      ? Math.max(...ty.members.map(m => m.offset + elementStride(m.ty) * Math.max(m.ty.elements, 1)))
      : 16;
  } else if (ty.class === 2) bytes = ty.rows * 16;
  else if (ty.class === 3) bytes = ty.columns * 16;
  else bytes = Math.max(ty.columns, 1) * 4;
  return Math.ceil(bytes / 16) * 16;
}

/** The name of the 4-byte value at `offset` in a variable of type `ty` called `name`, and its base type. */
function nameIn(name: string, ty: Type, offset: number): [string, number] {
  if (ty.elements > 0) {
    const element = { ...ty, elements: 0 };
    const stride = elementStride(element);
    const i = Math.min(Math.floor(offset / stride), ty.elements - 1);
    return nameIn(`${name}[${i}]`, element, offset - i * stride);
  }
  switch (ty.class) {
    case 5: {
      const m = [...ty.members].reverse().find(m => m.offset <= offset);
      return m ? nameIn(`${name}.${m.name}`, m.ty, offset - m.offset) : [name, ty.base];
    }
    case 2: case 3: {
      const major = Math.floor(offset / 16), minor = Math.floor((offset % 16) / 4);
      const [r, c] = ty.class === 2 ? [major, minor] : [minor, major];
      return [`${name}[${r}][${c}]`, ty.base];
    }
    case 1:
      return [`${name}.${COMPONENTS[Math.floor(offset / 4) % 4]}`, ty.base];
    default:
      return [name, ty.base];
  }
}

/** The name of the 4-byte value at `offset` bytes into the buffer and its base type, if a variable covers it. */
export function nameAt(cb: ConstantBuffer, offset: number): { name: string; base: number; used: boolean } | null {
  const v = cb.variables.find(v => offset >= v.offset && offset < v.offset + v.size);
  if (!v) return null;
  const [name, base] = nameIn(v.name, v.ty, offset - v.offset);
  return { name, base, used: v.used };
}
