// LZ4 blocks, as locum's blob store writes a compressed blob (codec `lz4`: one raw block, no frame and no size
// prefix; the store's index gives the decoded size).

/** `input`, one LZ4 block, decoded to exactly `size` bytes. Refuses a block that reads past its end, writes past
 * `size`, refers to bytes before the output's start, or decodes to fewer bytes than `size`. */
export function lz4Block(input: Uint8Array, size: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(size);
  let i = 0;
  let o = 0;
  const fail = (why: string): never => {
    throw new Error(`lz4 block: ${why} (input byte ${i})`);
  };
  // A length's 15 is continued by bytes added to it, 255 meaning another follows.
  const extend = (n: number): number => {
    if (n !== 15) return n;
    let b: number;
    do {
      if (i >= input.length) fail("a length runs past the block's end");
      b = input[i++]!;
      n += b;
    } while (b === 255);
    return n;
  };
  while (i < input.length) {
    const token = input[i++]!;
    const literals = extend(token >> 4);
    if (i + literals > input.length) fail("literals run past the block's end");
    if (o + literals > size) fail(`literals run past the ${size} bytes it decodes to`);
    out.set(input.subarray(i, i + literals), o);
    i += literals;
    o += literals;
    // The last sequence is literals alone.
    if (i === input.length) break;
    if (i + 2 > input.length) fail("a match's offset runs past the block's end");
    const offset = input[i]! | (input[i + 1]! << 8);
    i += 2;
    if (offset === 0 || offset > o) fail(`match offset ${offset} reaches before the output's start`);
    const length = extend(token & 15) + 4;
    if (o + length > size) fail(`a match runs past the ${size} bytes it decodes to`);
    if (offset >= length) {
      out.copyWithin(o, o - offset, o - offset + length);
      o += length;
    } else {
      // The match overlaps what it writes: a repeat of its last `offset` bytes, copied one at a time.
      for (const end = o + length; o < end; o++) out[o] = out[o - offset]!;
    }
  }
  if (o !== size) fail(`decoded ${o} bytes, not ${size}`);
  return out;
}
