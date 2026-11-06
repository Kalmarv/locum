import { expect, test } from "bun:test";
import { sourceWords } from "../src/util";

test("every content source a capture writes is said in words", () => {
  for (const source of ["first-use", "upload", "upload-at-first-submission", "after-frame", "tile-events"])
    expect(sourceWords(source)).not.toBe(source);
  expect(sourceWords("after-frame")).toContain("ExecuteIndirect argument records");
  expect(sourceWords(null)).toBe("none");
  // One a later locum adds is shown as named, not dropped.
  expect(sourceWords("later")).toBe("later");
});
