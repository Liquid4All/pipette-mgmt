// RFC 9421 request signatures in the server's one profile (`docs/authentication.md`
// §2.4): Ed25519 over the method, authority, path, query and, with a body, its
// RFC 9530 `Content-Digest`. `crates/pipette-httpsig` is the verifier.

const COMPONENTS = ["@method", "@authority", "@path", "@query"];

/** A parameter's value: the profile's are strings and integers. */
export type Param = string | number;

const quote = (s: string) => `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;

/** The covered components and their parameters, serialized as RFC 9651 does. */
export function serializeParams(components: string[], params: [string, Param][]): string {
  const list = `(${components.map(quote).join(" ")})`;
  return list + params.map(([key, value]) => `;${key}=${typeof value === "number" ? value : quote(value)}`).join("");
}

/** The RFC 9421 §2.5 signature base. */
export function signatureBase(components: [string, string][], params: string): string {
  return components.map(([name, value]) => `"${name}": ${value}\n`).join("") + `"@signature-params": ${params}`;
}

export const base64 = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes));

/** `Content-Digest` for a body whose SHA-256 is `sha256`. */
export const contentDigest = (sha256: Uint8Array) => `sha-256=:${base64(sha256)}:`;

export async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes as BufferSource));
}

/** Sign a request; `digest` is the body's SHA-256, `null` for no body. */
export async function signRequest(privateKey: CryptoKey, keyId: string, method: string, authority: string,
                                  pathAndQuery: string, digest: Uint8Array | null, created: number,
                                  nonce: string): Promise<Record<string, string>> {
  const at = pathAndQuery.indexOf("?");
  const path = at < 0 ? pathAndQuery : pathAndQuery.slice(0, at);
  const query = at < 0 ? "?" : pathAndQuery.slice(at);
  const lines: [string, string][] = [
    ["@method", method], ["@authority", authority.toLowerCase()], ["@path", path], ["@query", query],
  ];
  const components = [...COMPONENTS];
  const fields: Record<string, string> = {};
  if (digest) {
    const field = contentDigest(digest);
    fields["Content-Digest"] = field;
    components.push("content-digest");
    lines.push(["content-digest", field]);
  }
  const params = serializeParams(components, [
    ["created", created], ["nonce", nonce], ["keyid", keyId], ["alg", "ed25519"], ["tag", "pipette"],
  ]);
  const signature = await crypto.subtle.sign("Ed25519", privateKey, new TextEncoder().encode(signatureBase(lines, params)));
  return { ...fields, "Signature-Input": `sig=${params}`, "Signature": `sig=:${base64(new Uint8Array(signature))}:` };
}
