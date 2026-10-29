import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { ApiError, me } from "../src/lib/api";
import { generateKey, keyId, unhex, type GeneratedKey } from "../src/lib/keys";
import { BROWSER_KEY_DAYS, signInWithAddedKey, signInWithOwnKey } from "../src/lib/signin";
import { keyIdOf, verifies } from "./verify";

const USER = "user_0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000";
const DETAILS = "console app, Chrome on macOS";
const DAY = 86_400_000;

interface Registration {
  publicKey: string;
  key_details: string;
  expires_at?: string;
}

const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });

/** One account's keys behind the routes sign-in calls, refusing what the server refuses. */
class FakeServer {
  readonly keys = new Map<string, Registration>();
  readonly calls: string[] = [];

  /** As `pipette-ctl auth keys add` would, with `--expires-in` when `expires_at` is given. */
  register(key: GeneratedKey, expires_at?: string) {
    this.keys.set(key.key_id, { publicKey: key.publicKey, key_details: "laptop", expires_at });
  }

  fetch = async (path: string, init: RequestInit = {}): Promise<Response> => {
    const method = init.method ?? "GET";
    const headers = init.headers as Record<string, string>;
    const signer = keyIdOf(headers) ?? "";
    this.calls.push(`${method} ${path} by ${signer}`);

    // The key names the account, so a signature verifying is the whole check.
    const key = this.keys.get(signer);
    if (!key || !(await verifies(key.publicKey, method, path, headers, "", init.body as string | undefined))) {
      return reply(401, { error: "invalid signature" });
    }
    if (method === "GET" && path === "/api/v1/users/me") {
      const keys = [...this.keys].map(([key_id, { key_details, expires_at }]) => ({ key_id, key_details, expires_at }));
      return reply(200, { user_id: USER, keys });
    }
    if (method === "POST" && path === "/api/v1/users/me/keys") {
      const { public_key, key_details, expires_at } = JSON.parse(init.body as string);
      const key_id = await keyId(unhex(public_key));
      this.keys.set(key_id, { publicKey: public_key, key_details, expires_at });
      return reply(201, { user_id: USER, status: "active", key_id });
    }
    const revoked = /^\/api\/v1\/users\/me\/keys\/([^/]+)$/.exec(path)?.[1];
    if (method === "DELETE" && revoked && this.keys.delete(decodeURIComponent(revoked))) {
      return new Response(null, { status: 204 });
    }
    return reply(404, { error: "no such route" });
  };
}

let server: FakeServer;

beforeEach(() => {
  server = new FakeServer();
  vi.stubGlobal("fetch", server.fetch);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Asserts `key_id` is registered to expire `BROWSER_KEY_DAYS` after a time in [before, after]. */
function expectBrowserKey(key_id: string, before: number, after: number) {
  const registration = server.keys.get(key_id);
  expect(registration?.key_details).toBe(DETAILS);
  const at = Date.parse(registration?.expires_at ?? "");
  expect(at).toBeGreaterThanOrEqual(before + BROWSER_KEY_DAYS * DAY);
  expect(at).toBeLessThanOrEqual(after + BROWSER_KEY_DAYS * DAY);
}

describe("signInWithOwnKey", () => {
  test("keeps an expiring key of the browser's own, and leaves the reader's key registered", async () => {
    const own = await generateKey();
    server.register(own);

    const before = Date.now();
    const identity = await signInWithOwnKey(own.seed, DETAILS);
    const after = Date.now();

    expect(identity.user_id).toBe(USER);
    expect(identity.origin).toBe("browser");
    expect(identity.key_id).not.toBe(own.key_id);
    expect(identity.privateKey.extractable).toBe(false);
    expectBrowserKey(identity.key_id, before, after);
    expect(server.keys.get(own.key_id)?.expires_at).toBeUndefined();
    expect(server.calls).toEqual([
      `GET /api/v1/users/me by ${own.key_id}`,
      `POST /api/v1/users/me/keys by ${own.key_id}`,
    ]);
  });

  test("the key it keeps signs as the account", async () => {
    const own = await generateKey();
    server.register(own);

    const identity = await signInWithOwnKey(own.seed, DETAILS);

    expect((await me(identity)).user_id).toBe(USER);
    expect(server.calls.at(-1)).toBe(`GET /api/v1/users/me by ${identity.key_id}`);
  });

  test("a key on no account registers nothing", async () => {
    const stranger = await generateKey();

    const signingIn = signInWithOwnKey(stranger.seed, DETAILS);

    await expect(signingIn).rejects.toBeInstanceOf(ApiError);
    await expect(signingIn).rejects.toMatchObject({ status: 401 });
    expect(server.keys.size).toBe(0);
    expect(server.calls).toEqual([`GET /api/v1/users/me by ${stranger.key_id}`]);
  });

  test("text that is not a private key calls nothing", async () => {
    await expect(signInWithOwnKey("not a key", DETAILS)).rejects.toThrow(/not an Ed25519 private key/);
    expect(server.calls).toEqual([]);
  });
});

describe("signInWithAddedKey", () => {
  test("keeps the key the command added with an expiry, and registers nothing", async () => {
    const added = await generateKey();
    server.register(added, new Date(Date.now() + BROWSER_KEY_DAYS * DAY).toISOString());

    const identity = await signInWithAddedKey(added, DETAILS);

    expect(identity).toEqual({ key_id: added.key_id, privateKey: added.privateKey, user_id: USER, origin: "browser" });
    expect(server.keys.size).toBe(1);
    expect(server.calls).toEqual([`GET /api/v1/users/me by ${added.key_id}`]);
  });

  test("swaps a key added without an expiry for an expiring one, signing the revocation with it", async () => {
    const added = await generateKey();
    server.register(added);

    const before = Date.now();
    const identity = await signInWithAddedKey(added, DETAILS);
    const after = Date.now();

    expect(identity.user_id).toBe(USER);
    expect(identity.origin).toBe("browser");
    expectBrowserKey(identity.key_id, before, after);
    expect(server.keys.has(added.key_id)).toBe(false);
    expect(server.calls).toEqual([
      `GET /api/v1/users/me by ${added.key_id}`,
      `POST /api/v1/users/me/keys by ${added.key_id}`,
      `DELETE /api/v1/users/me/keys/${added.key_id} by ${identity.key_id}`,
    ]);
  });

  test("a key the command has not added yet says to run it", async () => {
    const added = await generateKey();

    await expect(signInWithAddedKey(added, DETAILS)).rejects.toThrow(/not on an account yet\. Run the command/);
    expect(server.keys.size).toBe(0);
  });
});
