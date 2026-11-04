import { expect, test } from "bun:test";
import { parseRoute } from "../src/util";

test("pipeline/draw/N names a draw by the number locum-cli uses", () => {
  const r = parseRoute("#/c/a33b8d77d2b0/pipeline/draw/2775");
  expect([r.id, r.tab, r.item, r.sub]).toEqual(["a33b8d77d2b0", "pipeline", "draw", "2775"]);
});

test("pipeline/SEQ names an event", () => {
  const r = parseRoute("#/c/a33b8d77d2b0/pipeline/23357");
  expect([r.tab, r.item, r.sub]).toEqual(["pipeline", "23357", null]);
});
