// Sign pipette-mgmt requests as an account: RFC 9421 with Ed25519, the profile the server verifies
// (`docs/authentication.md` §2.4). No dependencies: Node >= 22.18, bun or deno.
//
//   import { signerFromEnv, signedFetch } from "./tools/sign.ts";
//   const signer = await signerFromEnv();
//   const res = await signedFetch(signer, "https://collector.pipette.liquid.ai/api/v1/users/me");
//
// Or print the headers for curl, one signature per request:
//
//   curl -H @<(node tools/sign.ts "$URL") "$URL"                                      (bash, zsh)
//   curl -H @<(node tools/sign.ts "$URL" POST body.json) --data-binary @body.json "$URL"
//   curl -H @(node tools/sign.ts $URL | psub) $URL                                     (fish)
//
// PIPETTE_KEY picks the key:
//   op://vault/item/private key    a 1Password item, read with the `op` CLI
//   agent[:<match>]                an ssh-agent key at SSH_AUTH_SOCK (the 1Password agent too): the
//                                  Ed25519 key whose SHA256 fingerprint or comment contains <match>,
//                                  or the only Ed25519 key it holds
//   <path>                         a private key file, PKCS#8 PEM or unencrypted OpenSSH
import { createHash, createPrivateKey, createPublicKey, randomBytes, sign, type KeyObject } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { createConnection } from "node:net";
import { fileURLToPath } from "node:url";

/** Signs with one account key. `publicKey` is the raw 32 bytes. */
export interface Signer {
  publicKey: Buffer;
  sign(data: Buffer): Buffer | Promise<Buffer>;
}

/** `userkey_` + hex SHA-256 of the raw public key: the `keyid` the server looks the account up by. */
export const keyId = (signer: Signer): string =>
  "userkey_" + createHash("sha256").update(signer.publicKey).digest("hex");

export function keySigner(key: KeyObject): Signer {
  if (key.asymmetricKeyType !== "ed25519") throw new Error("not an Ed25519 key");
  const spki = createPublicKey(key).export({ format: "der", type: "spki" });
  return { publicKey: spki.subarray(-32), sign: (data) => sign(null, data, key) };
}

/** A private key's text, PKCS#8 PEM (what `op read` gives) or unencrypted OpenSSH. */
export function textSigner(text: string): Signer {
  if (!text.includes("OPENSSH PRIVATE KEY")) return keySigner(createPrivateKey(text));
  const r = sshReader(Buffer.from(text.replace(/-----[^-]+-----/g, "").replace(/\s+/g, ""), "base64"));
  r.skip("openssh-key-v1\0".length);
  if (r.string().toString() !== "none") throw new Error("the key is passphrase-protected; make one with -N \"\"");
  r.string(); r.string(); r.skip(4); r.string(); // kdfname, kdfoptions, key count, public key
  const priv = sshReader(r.string());
  priv.skip(8); // two check ints
  if (priv.string().toString() !== "ssh-ed25519") throw new Error("not an Ed25519 key");
  priv.string(); // public key
  const seed = priv.string().subarray(0, 32);
  const der = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]); // PKCS#8 around the seed
  return keySigner(createPrivateKey({ key: der, format: "der", type: "pkcs8" }));
}

export const fileSigner = (path: string): Signer => textSigner(readFileSync(path, "utf8"));

/** A 1Password SSH key item, e.g. `op://Employee/k1/private key`. Needs the `op` CLI signed in. */
export const onePasswordSigner = (ref: string): Signer =>
  textSigner(execFileSync("op", ["read", ref], { encoding: "utf8" }));

/** An Ed25519 key held by the ssh-agent at `socket`; `match` picks by fingerprint or comment. */
export async function agentSigner(match = "", socket = process.env.SSH_AUTH_SOCK): Promise<Signer> {
  if (!socket) throw new Error("SSH_AUTH_SOCK is not set");
  const answer = sshReader(await agentRequest(socket, Buffer.from([11]))); // REQUEST_IDENTITIES
  if (answer.byte() !== 12) throw new Error("the agent refused to list its keys");
  const keys = [];
  for (let n = answer.uint32(); n > 0; n--) {
    const blob = answer.string();
    const comment = answer.string().toString();
    const fingerprint = "SHA256:" + createHash("sha256").update(blob).digest("base64").replace(/=+$/, "");
    const key = sshReader(blob);
    if (key.string().toString() === "ssh-ed25519") keys.push({ blob, comment, fingerprint, publicKey: key.string() });
  }
  const picked = keys.filter((k) => k.fingerprint.includes(match) || k.comment.includes(match));
  if (picked.length !== 1) {
    const listed = keys.map((k) => `${k.fingerprint} ${k.comment}`).join("; ") || "none";
    throw new Error(`${picked.length} Ed25519 agent keys match "${match}" (held: ${listed})`);
  }
  const { blob, publicKey } = picked[0];
  return {
    publicKey,
    async sign(data) {
      const reply = sshReader(await agentRequest(socket, Buffer.concat([Buffer.from([13]), sshString(blob), sshString(data), uint32(0)])));
      if (reply.byte() !== 14) throw new Error("the agent refused to sign");
      const sig = sshReader(reply.string());
      sig.string(); // "ssh-ed25519"
      return sig.string();
    },
  };
}

/** The signer PIPETTE_KEY names (see the header); `fallback` when it is unset. */
export async function signerFromEnv(fallback?: string): Promise<Signer> {
  const source = process.env.PIPETTE_KEY || fallback;
  if (!source) throw new Error("set PIPETTE_KEY to an op:// reference, agent[:<match>], or a key file");
  if (source.startsWith("op://")) return onePasswordSigner(source);
  if (source === "agent" || source.startsWith("agent:")) return agentSigner(source.slice("agent:".length));
  return fileSigner(source);
}

/** The `Signature-Input`, `Signature` and, with a body, `Content-Digest` headers for one request. */
export async function signRequest(
  signer: Signer, method: string, url: string | URL, body?: Uint8Array,
  created = Math.floor(Date.now() / 1000), nonce = randomBytes(12).toString("hex"),
): Promise<Record<string, string>> {
  const u = new URL(url);
  const headers: Record<string, string> = {};
  const components: [string, string][] = [
    ["@method", method.toUpperCase()],
    ["@authority", u.host.toLowerCase()],
    ["@path", u.pathname],
    ["@query", u.search || "?"],
  ];
  if (body) {
    headers["Content-Digest"] = `sha-256=:${createHash("sha256").update(body).digest("base64")}:`;
    components.push(["content-digest", headers["Content-Digest"]]);
  }
  const params = `(${components.map(([n]) => `"${n}"`).join(" ")});created=${created};` +
    `nonce="${nonce}";keyid="${keyId(signer)}";alg="ed25519";tag="pipette"`;
  const base = components.map(([n, v]) => `"${n}": ${v}\n`).join("") + `"@signature-params": ${params}`;
  headers["Signature-Input"] = `sig=${params}`;
  headers["Signature"] = `sig=:${Buffer.from(await signer.sign(Buffer.from(base))).toString("base64")}:`;
  return headers;
}

/** `fetch`, signed. A string body is sent as UTF-8 and signed as such. */
export async function signedFetch(
  signer: Signer, url: string | URL, init: { method?: string; body?: string | Uint8Array; headers?: Record<string, string> } = {},
): Promise<Response> {
  const method = init.method ?? (init.body === undefined ? "GET" : "POST");
  const body = typeof init.body === "string" ? Buffer.from(init.body) : init.body;
  const signed = await signRequest(signer, method, url, body);
  return fetch(url, { method, body, headers: { ...(body ? { "Content-Type": "application/json" } : {}), ...init.headers, ...signed } });
}

function agentRequest(socket: string, message: Buffer): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const conn = createConnection(socket);
    let got = Buffer.alloc(0);
    conn.on("error", reject);
    conn.on("end", () => reject(new Error("the agent closed the connection")));
    conn.on("data", (chunk) => {
      got = Buffer.concat([got, chunk]);
      if (got.length >= 4 && got.length >= 4 + got.readUInt32BE(0)) {
        conn.end();
        resolve(got.subarray(4, 4 + got.readUInt32BE(0)));
      }
    });
    conn.write(Buffer.concat([uint32(message.length), message]));
  });
}

function sshReader(buf: Buffer) {
  let at = 0;
  return {
    skip: (n: number) => { at += n; },
    byte: () => buf[at++],
    uint32: () => { const v = buf.readUInt32BE(at); at += 4; return v; },
    string: () => { const n = buf.readUInt32BE(at); at += 4; return buf.subarray(at, (at += n)); },
  };
}

const uint32 = (v: number) => { const b = Buffer.alloc(4); b.writeUInt32BE(v); return b; };
const sshString = (b: Buffer) => Buffer.concat([uint32(b.length), b]);

if (fileURLToPath(import.meta.url) === process.argv[1]) {
  try {
    const [url, method = "GET", bodyFile] = process.argv.slice(2);
    if (!url) throw new Error("usage: node sign.ts <url> [method] [body-file]");
    const headers = await signRequest(await signerFromEnv(), method, url, bodyFile ? readFileSync(bodyFile) : undefined);
    for (const [name, value] of Object.entries(headers)) console.log(`${name}: ${value}`);
  } catch (e) {
    console.error((e as Error).message);
    process.exit(1);
  }
}
