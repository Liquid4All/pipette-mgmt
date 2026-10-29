import { describe, expect, test } from "vitest";
import { memberKeyOf, nameOf, nameTexts, synonymsTitle } from "../src/lib/descriptor-names";

const CUDA = { display: "global/llamacpp-v0.7.0::linux-cuda-12-8", name: "global/llamacpp-v0.7.0",
               synonyms: ["b11669::linux-cuda-12-8", "llamacpp-daily::linux-cuda-12-8"] };

describe("a descriptor's catalog names", () => {
  test("part the member key from the name, which the console draws quieter", () => {
    expect(memberKeyOf(CUDA)).toBe("::linux-cuda-12-8");
    expect(memberKeyOf({ display: "custom-build", name: "custom-build", synonyms: [] })).toBe("");
  });

  test("list the synonyms in the tooltip, one on each line, and nothing where there are none", () => {
    expect(synonymsTitle(CUDA)).toBe("Also named:\nb11669::linux-cuda-12-8\nllamacpp-daily::linux-cuda-12-8");
    expect(synonymsTitle({ ...CUDA, synonyms: [] })).toBe("");
  });

  test("are looked up by digest, none for a digest or a read with none", () => {
    const names = { ["a".repeat(64)]: CUDA };
    expect(nameOf(names, "a".repeat(64))).toBe(CUDA);
    expect([nameOf(names, "b".repeat(64)), nameOf(names, null), nameOf(undefined, "a".repeat(64))])
      .toEqual([null, null, null]);
    expect(nameTexts(CUDA)).toEqual([CUDA.display, ...CUDA.synonyms]);
    expect(nameTexts(null)).toEqual([]);
  });
});
