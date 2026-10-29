import { describe, expect, test } from "vitest";
import { signedHeaders } from "../src/lib/api";
import { generateKey, hex, importKey, pastedPublicKey, unhex } from "../src/lib/keys";
import { sha256 } from "../src/lib/httpsig";
import { nonceOf, verifies } from "./verify";

const USER = "user_0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000";
const AT = new Date("2026-10-02T12:00:00.000Z");

describe("keys", () => {
  test("a generated key's id is userkey_ plus the sha256 of its public half", async () => {
    const key = await generateKey();
    const digest = hex(await crypto.subtle.digest("SHA-256", unhex(key.publicKey)));
    expect(key.key_id).toBe(`userkey_${digest}`);
    expect(key.seed).toMatch(/^[0-9a-f]{64}$/);
  });

  test("its saved seed imports back to the same key id, non-extractable", async () => {
    const key = await generateKey();
    const imported = await importKey(`${key.seed}\n`);
    expect(imported.key_id).toBe(key.key_id);
    expect(imported.privateKey.extractable).toBe(false);
    expect(key.privateKey.extractable).toBe(false);
  });

  test("anything but an Ed25519 key is refused with what was expected", async () => {
    await expect(importKey("not a key")).rejects.toThrow(/id_ed25519/);
  });
});

describe("signedHeaders", () => {
  test("signs RFC 9421 over method, authority, path and query, naming only the key", async () => {
    const key = await generateKey();
    const headers = await signedHeaders({ ...key, user_id: USER }, "GET", "/api/v1/plans?limit=5", null, AT,
                                        "Mgmt.Example");

    expect(Object.keys(headers).sort()).toEqual(["Signature", "Signature-Input"]);
    expect(headers["Signature-Input"]).toBe(
      `sig=("@method" "@authority" "@path" "@query");created=${AT.getTime() / 1000};nonce="${nonceOf(headers)}"`
      + `;keyid="${key.key_id}";alg="ed25519";tag="pipette"`);
    expect(nonceOf(headers)).toMatch(/^[0-9a-f]{32}$/);
    expect(await verifies(key.publicKey, "GET", "/api/v1/plans?limit=5", headers, "mgmt.example")).toBe(true);
    expect(await verifies(key.publicKey, "GET", "/api/v1/plans?limit=6", headers, "mgmt.example")).toBe(false);
  });

  test("a body is covered by its Content-Digest", async () => {
    const key = await generateKey();
    const body = '{"slug":"acme"}';
    const headers = await signedHeaders({ ...key, user_id: null }, "POST", "/api/v1/orgs",
                                        await sha256(new TextEncoder().encode(body)), AT);

    expect(headers["Signature-Input"]).toContain('"@query" "content-digest")');
    expect(await verifies(key.publicKey, "POST", "/api/v1/orgs", headers, "", body)).toBe(true);
    expect(await verifies(key.publicKey, "POST", "/api/v1/orgs", headers, "", '{"slug":"evil"}')).toBe(false);
  });

  test("every request carries a fresh nonce", async () => {
    const key = await generateKey();
    const signer = { ...key, user_id: USER };
    const [a, b] = await Promise.all([signedHeaders(signer, "GET", "/api/v1/users/me", null, AT),
                                      signedHeaders(signer, "GET", "/api/v1/users/me", null, AT)]);
    expect(nonceOf(a)).not.toBe(nonceOf(b));
  });
});

describe("pastedPublicKey", () => {
  // The vector `src/pasted_key.rs` tests against: one ssh-keygen key in both spellings.
  const B64 = "AAAAC3NzaC1lZDI1NTE5AAAAIDORQnan1azvdRvCwu/kaTwsMNxnJVdE5xaM7PSfN4Hl";
  const HEX = "33914276a7d5acef751bc2c2efe4693c2c30dc67255744e7168cecf49f3781e5";

  test.each([
    HEX, `  ${HEX}\n`, HEX.toUpperCase(),
    `ssh-ed25519 ${B64} me@laptop`, `ssh-ed25519 ${B64}`, `ssh-ed25519\t${B64}\tme@laptop\n`,
  ])("%j reads as its hex", (raw) => {
    expect(pastedPublicKey(raw)).toBe(HEX);
  });

  test.each([
    ["ssh-rsa AAAAB3NzaC1yc2E me@laptop", /a ssh-rsa key cannot sign here/],
    [`sk-ssh-ed25519@openssh.com ${B64}`, /cannot sign here/],
    ["ssh-ed25519", /does not carry a key/],
    ["ssh-ed25519 !!!!", /does not carry a key/],
    [`ssh-ed25519 ${B64.slice(0, -4)}`, /does not carry a key/],
  ])("%j is refused", (raw, says) => {
    expect(() => pastedPublicKey(raw)).toThrow(says);
  });
});
