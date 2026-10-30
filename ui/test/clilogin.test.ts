import { afterEach, describe, expect, test, vi } from "vitest";
import { ApiError, type Signer } from "../src/lib/api";
import { approve, cliRequest, codeComplete, codeMatches, expiresAt, keyNames } from "../src/lib/clilogin";
import { generateKey, hex, unhex } from "../src/lib/keys";

afterEach(() => vi.unstubAllGlobals());

// RFC 8032 §7.1, test 1.
const RFC_KEY = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const AT = new Date("2026-10-08T12:00:00.000Z");

const query = (search: string) => new URLSearchParams(search);
const sha256 = async (bytes: Uint8Array<ArrayBuffer>) => new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));

describe("the link's request", () => {
  test("is the key and what the machine says it is, percent-decoded", () => {
    expect(cliRequest(query(`key=${RFC_KEY}&details=ana%40build-box.test`)))
      .toEqual({ public_key: RFC_KEY, key_details: "ana@build-box.test" });
  });

  test("lowercases a key typed in capitals", () => {
    expect(cliRequest(query(`key=${RFC_KEY.toUpperCase()}&details=laptop`))?.public_key).toBe(RFC_KEY);
  });

  test("is none without 64 hex characters or a description", () => {
    for (const search of [
      "details=laptop", `key=${RFC_KEY.slice(2)}&details=laptop`, `key=${RFC_KEY}00&details=laptop`,
      `key=${RFC_KEY.slice(1)}g&details=laptop`, `key=%20${RFC_KEY}&details=laptop`, `key=${RFC_KEY}`,
      `key=${RFC_KEY}&details=%20%20`,
    ]) {
      expect(cliRequest(query(search)), search).toBeNull();
    }
  });
});

describe("the key's names", () => {
  test("the code is the first 8 hex characters of the key's digest, uppercased", async () => {
    const names = await keyNames(RFC_KEY);
    expect(names.key_id).toBe(`userkey_${hex(await sha256(unhex(RFC_KEY)))}`);
    expect(names.key_id.startsWith("userkey_21fe31df")).toBe(true);
    expect(names.code).toBe("21FE-31DF");
  });

  test("the fingerprint is OpenSSH's, over the ssh-ed25519 blob", async () => {
    // RFC 4253 §6.6: the type name, then the key, each after its 32-bit big-endian length.
    const type = new TextEncoder().encode("ssh-ed25519");
    const blob = new Uint8Array(4 + type.length + 4 + 32);
    const view = new DataView(blob.buffer);
    view.setUint32(0, type.length);
    blob.set(type, 4);
    view.setUint32(4 + type.length, 32);
    blob.set(unhex(RFC_KEY), 8 + type.length);
    const expected = `SHA256:${btoa(String.fromCharCode(...await sha256(blob))).replace(/=+$/, "")}`;
    // As `ssh-keygen -lf` prints it for this key.
    expect(expected).toBe("SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8");
    expect((await keyNames(RFC_KEY)).fingerprint).toBe(expected);
  });
});

describe("the typed code", () => {
  test("matches whatever its case, spaces and dashes", () => {
    for (const typed of ["21FE-31DF", "21fe31df", " 21fe 31df ", "21-fe-31-df", "21Fe-31dF\n"]) {
      expect(codeMatches(typed, "21FE-31DF"), typed).toBe(true);
    }
  });

  test("does not match another code, a part of it, or nothing", () => {
    for (const typed of ["", "21FE", "21FE-31DE", "21FE-31DF0", "31DF-21FE", "21FE_31DF"]) {
      expect(codeMatches(typed, "21FE-31DF"), typed).toBe(false);
    }
  });

  test("is complete at 8 characters, spaces and dashes aside", () => {
    for (const [typed, complete] of [
      ["", false], ["21FE-31D", false], ["21 FE-31 D-", false], ["21FE-31DE", true], ["21-fe 31-de", true],
      ["21FE-31DF0", true],
    ] as const) {
      expect(codeComplete(typed), typed).toBe(complete);
    }
  });
});

describe("the expiry", () => {
  test("is none by default, or 30 days on", () => {
    expect(expiresAt("never", AT)).toBeUndefined();
    expect(expiresAt("30d", AT)).toBe("2026-11-07T12:00:00.000Z");
  });
});

describe("approving", () => {
  const request = { public_key: RFC_KEY, key_details: "ana@build-box.test" };

  async function signer(): Promise<Signer> {
    const key = await generateKey();
    return { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
  }

  test("posts the key, its details and the expiry to the account's keys", async () => {
    const fetched = vi.fn(async (_path: string, _init?: RequestInit) =>
      new Response(JSON.stringify({ user_id: "user_x", key_id: "userkey_21fe" }), { status: 201 }));
    vi.stubGlobal("fetch", fetched);

    expect(await approve(await signer(), request, "30d", AT)).toBe("approved");
    expect(await approve(await signer(), request, "never", AT)).toBe("approved");

    const [[path, init], [, never]] = fetched.mock.calls;
    expect(path).toBe("/api/v1/users/me/keys");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init?.body as string)).toEqual({ ...request, expires_at: "2026-11-07T12:00:00.000Z" });
    expect(JSON.parse(never?.body as string)).toEqual(request);
  });

  test("names a key another holds, and throws any other refusal", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ error: "key held" }), { status: 409 })));
    expect(await approve(await signer(), request, "never", AT)).toBe("taken");

    vi.stubGlobal("fetch", vi.fn(async () =>
      new Response(JSON.stringify({ error: "expires_at is more than 30 days away" }), { status: 400 })));
    const refused = approve(await signer(), request, "30d", AT);
    await expect(refused).rejects.toBeInstanceOf(ApiError);
    await expect(refused).rejects.toMatchObject({ status: 400 });
    await expect(refused).rejects.toThrow("400: expires_at is more than 30 days away");
  });
});
