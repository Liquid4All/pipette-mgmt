import { contentDigest, sha256, signatureBase } from "../src/lib/httpsig";
import { unhex } from "../src/lib/keys";

const fromBase64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

/** The `keyid` an RFC 9421 `Signature-Input` names. */
export const keyIdOf = (headers: Record<string, string>) => /;keyid="([^"]*)"/.exec(headers["Signature-Input"] ?? "")?.[1];

/** The `nonce` an RFC 9421 `Signature-Input` names. */
export const nonceOf = (headers: Record<string, string>) => /;nonce="([^"]*)"/.exec(headers["Signature-Input"] ?? "")?.[1];

/**
 * Whether `headers` carry an RFC 9421 signature by `publicKey` (hex) over the
 * request, rebuilt as the server rebuilds it; with `body`, also that its
 * `Content-Digest` is that body's.
 */
export async function verifies(publicKey: string, method: string, pathAndQuery: string,
                               headers: Record<string, string>, authority = "", body?: string): Promise<boolean> {
  const input = headers["Signature-Input"] ?? "";
  const params = input.startsWith("sig=") ? input.slice(4) : "";
  const listed = /^\(([^)]*)\)/.exec(params)?.[1] ?? "";
  const at = pathAndQuery.indexOf("?");
  const values: Record<string, string | undefined> = {
    "@method": method,
    "@authority": authority.toLowerCase(),
    "@path": at < 0 ? pathAndQuery : pathAndQuery.slice(0, at),
    "@query": at < 0 ? "?" : pathAndQuery.slice(at),
    "content-digest": headers["Content-Digest"],
  };
  const lines = listed.split(" ").filter(Boolean).map((quoted) => {
    const name = quoted.slice(1, -1);
    return [name, values[name] ?? ""] as [string, string];
  });
  if (body !== undefined && headers["Content-Digest"] !== contentDigest(await sha256(new TextEncoder().encode(body)))) {
    return false;
  }
  const signature = /^sig=:([A-Za-z0-9+/=]+):$/.exec(headers["Signature"] ?? "")?.[1];
  if (!signature) return false;
  const key = await crypto.subtle.importKey("raw", unhex(publicKey), "Ed25519", false, ["verify"]);
  return crypto.subtle.verify("Ed25519", key, fromBase64(signature), new TextEncoder().encode(signatureBase(lines, params)));
}
