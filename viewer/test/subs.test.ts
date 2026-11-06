import { expect, test } from "bun:test";
import { textureSubs } from "../server/format";

test("a multisampled texture's subresource is its samples stacked, where capture/src/layout.rs puts them", () => {
  // 2x1 RGBA8, 2 slices of 4 samples: each slice's samples are 4 images of 8 bytes, one after another.
  const subs = textureSubs(28, 2, 1, 1, 1, 2, 4)!;
  expect(subs.map(s => [s.index, s.slice, s.width, s.height, s.rows, s.offset, s.size]))
    .toEqual([[0, 0, 2, 4, 4, 0, 32], [1, 1, 2, 4, 4, 32, 32]]);
  // One sample is the plain layout.
  expect(textureSubs(28, 2, 1, 1, 1, 2, 1)).toEqual(textureSubs(28, 2, 1, 1, 1, 2));
});
