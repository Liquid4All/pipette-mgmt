import { describe, expect, test } from "vitest";
import { contentDigest, serializeParams, sha256, signatureBase, signRequest } from "../src/lib/httpsig";

const fromBase64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

/** RFC 9421 Appendix B.1.4, `test-key-ed25519`. */
const RFC_KEY = {
  kty: "OKP", crv: "Ed25519",
  d: "n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU",
  x: "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs",
};

const importRfcKey = (usage: "sign" | "verify") => crypto.subtle.importKey(
  "jwk", usage === "sign" ? RFC_KEY : { kty: RFC_KEY.kty, crv: RFC_KEY.crv, x: RFC_KEY.x }, "Ed25519", false, [usage]);

describe("httpsig", () => {
  test("reproduces RFC 9421's ed25519 example (B.2.6) byte for byte", async () => {
    const components = ["date", "@method", "@path", "@authority", "content-type", "content-length"];
    const params = serializeParams(components, [["created", 1618884473], ["keyid", "test-key-ed25519"]]);
    const base = signatureBase([
      ["date", "Tue, 20 Apr 2021 02:07:55 GMT"], ["@method", "POST"], ["@path", "/foo"],
      ["@authority", "example.com"], ["content-type", "application/json"], ["content-length", "18"],
    ], params);
    expect(base).toBe([
      '"date": Tue, 20 Apr 2021 02:07:55 GMT',
      '"@method": POST',
      '"@path": /foo',
      '"@authority": example.com',
      '"content-type": application/json',
      '"content-length": 18',
      '"@signature-params": ("date" "@method" "@path" "@authority" "content-type" "content-length");created=1618884473;keyid="test-key-ed25519"',
    ].join("\n"));
    const signature = fromBase64("wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw==");
    expect(await crypto.subtle.verify("Ed25519", await importRfcKey("verify"), signature, new TextEncoder().encode(base)))
      .toBe(true);
  });

  test("reproduces RFC 9530's sha-256 example", async () => {
    expect(contentDigest(await sha256(new TextEncoder().encode('{"hello": "world"}'))))
      .toBe("sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:");
  });

  // The vector `crates/pipette-httpsig/src/lib.rs` pins, so the two signers cannot drift.
  test("reproduces the server's shared vector", async () => {
    const body = new TextEncoder().encode('{"slug":"acme"}');
    const headers = await signRequest(await importRfcKey("sign"),
      "userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062", "POST", "mgmt.example",
      "/api/v1/orgs?dry_run=true", await sha256(body), 1760000000, "4f1c0de5a9b7");
    expect(headers).toEqual({
      "Content-Digest": "sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:",
      "Signature-Input": 'sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062";alg="ed25519";tag="pipette"',
      "Signature": "sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:",
    });
  });
});
