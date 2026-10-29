import { describe, expect, test } from "vitest";
import { hex } from "../src/lib/keys";
import { Sha256, sha256Of } from "../src/lib/sha256";

const encode = (text: string) => new TextEncoder().encode(text);

const digest = (...chunks: Uint8Array[]) => {
  const hash = new Sha256();
  for (const chunk of chunks) hash.update(chunk);
  return hash.hex();
};

/** WebCrypto's digest of the whole buffer, which the chunked one must equal. */
const whole = async (data: Uint8Array) => hex(await crypto.subtle.digest("SHA-256", data as BufferSource));

/** Bytes that are not periodic, so a chunk misplaced by a block shows. */
const bytes = (length: number) => Uint8Array.from({ length }, (_, i) => (i * 131 + (i >> 8) * 7) & 0xff);

describe("Sha256", () => {
  test.each([
    ["", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"],
    ["abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"],
    ["abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
      "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"],
    ["abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
      "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"],
  ])("digests the FIPS 180-4 vector %#", (text, expected) => {
    expect(digest(encode(text))).toBe(expected);
  });

  test("digests a million a's fed in uneven chunks", () => {
    const hash = new Sha256();
    const a = new Uint8Array(1_000_000).fill(0x61);
    for (let at = 0, step = 1; at < a.length; at += step, step = (step * 7) % 997 + 1) {
      hash.update(a.subarray(at, Math.min(at + step, a.length)));
    }
    expect(hash.hex()).toBe("cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
  });

  test("agrees with a one-shot digest at every length around the padding and block edges", async () => {
    for (const length of [0, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 1000]) {
      const data = bytes(length);
      const expected = await whole(data);
      expect(digest(data), `${length} bytes at once`).toBe(expected);
      for (const split of [1, 55, 56, 63, 64, 65]) {
        if (split > length) continue;
        expect(digest(data.subarray(0, split), data.subarray(split)), `${length} split at ${split}`).toBe(expected);
      }
      const bytewise = data.length ? Array.from(data, (byte) => Uint8Array.of(byte)) : [];
      expect(digest(...bytewise), `${length} a byte at a time`).toBe(expected);
    }
  });
});

describe("sha256Of", () => {
  test("hashes a file as it streams, reporting the bytes read", async () => {
    const data = bytes(300_000);
    const seen: number[] = [];
    const file = new File([data.slice(0, 100_000), data.slice(100_000)], "acme.gguf");
    expect(await sha256Of(file, (done) => seen.push(done))).toBe(await whole(data));
    expect(seen.length).toBeGreaterThan(1);
    expect(seen.at(-1)).toBe(300_000);
    expect(seen).toEqual([...seen].sort((a, b) => a - b));
  });

  test("stops reading when told", async () => {
    const gone = new AbortController();
    const seen: number[] = [];
    const file = new File([bytes(100_000), bytes(200_000)], "acme.gguf");
    const stopped = sha256Of(file, (done) => { seen.push(done); gone.abort(); }, gone.signal);
    await expect(stopped).rejects.toThrow("aborted");
    expect(seen).toHaveLength(1);
  });

  test("hashes an empty file", async () => {
    expect(await sha256Of(new File([], "empty.gguf")))
      .toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
  });
});
