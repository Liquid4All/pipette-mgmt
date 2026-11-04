import { describe, expect, test } from "vitest";
import type { DescriptorDisplay } from "../src/lib/api-types";
import { byCatalogName, memberKeyOf, nameOf, nameTexts, synonymsTitle } from "../src/lib/descriptor-names";

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

const named = (name: string, key = ""): DescriptorDisplay => ({ display: `${name}${key}`, name, synonyms: [] });

describe("a list of descriptors in catalog order", () => {
  const NAMES: Record<string, DescriptorDisplay> = {
    q16: named("LFM2-1.2B", "::gguf::q16_0"),
    q8: named("LFM2-1.2B", "::gguf::q8_0"),
    q4: named("LFM2-1.2B", "::gguf::q4_0"),
    small: named("LFM2-350M", "::gguf::q4_0"),
    qwen: named("qwen3-4B", "::gguf::q4_k_m"),
    global: named("global/llamacpp-v0.7.0", "::linux-x64"),
    org: named("llamacpp-v0.7.0", "::linux-x64"),
  };
  const sorted = (list: string[]) => {
    const { named: groups, unnamed } = byCatalogName(list, (descriptor) => NAMES[descriptor] ?? null);
    return { named: groups.map((group) => [group.name, group.members.map((m) => m.descriptor)]), unnamed };
  };

  test.each([
    { why: "groups an entry's members, in natural order of their member keys",
      list: ["q16", "q4", "q8"], named: [["LFM2-1.2B", ["q4", "q8", "q16"]]], unnamed: [] },
    { why: "orders the entries by name, ignoring case, numbers in natural order",
      list: ["qwen", "small", "q4"],
      named: [["LFM2-1.2B", ["q4"]], ["LFM2-350M", ["small"]], ["qwen3-4B", ["qwen"]]], unnamed: [] },
    { why: "keeps a global name and an organization's name of the same text apart",
      list: ["org", "global"],
      named: [["global/llamacpp-v0.7.0", ["global"]], ["llamacpp-v0.7.0", ["org"]]], unnamed: [] },
    { why: "puts the descriptors with no name after, by their text",
      list: ['{"repo":"b"}', "q4", '{"repo":"a"}'],
      named: [["LFM2-1.2B", ["q4"]]], unnamed: ['{"repo":"a"}', '{"repo":"b"}'] },
    { why: "gives nothing for no descriptor", list: [], named: [], unnamed: [] },
  ])("$why", ({ list, named: groups, unnamed }) => {
    expect(sorted(list)).toEqual({ named: groups, unnamed });
  });

  test("keeps each member's catalog names", () => {
    expect(byCatalogName(["q4"], (descriptor) => NAMES[descriptor] ?? null).named[0]?.members)
      .toEqual([{ descriptor: "q4", display: NAMES.q4 }]);
  });
});
