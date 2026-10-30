// Ed25519 keys as the server and `pipette-ctl` spell them, handed back
// non-extractable: a script on this origin can sign with one but not read it out.

const ALG = { name: "Ed25519" };
// PKCS#8 v1 wrapping of an Ed25519 seed is this fixed header plus the 32 seed
// bytes (RFC 8410 §7), so the two forms convert by slicing.
const PKCS8_PREFIX = "302e020100300506032b657004220420";

export interface SigningKey {
  key_id: string;
  privateKey: CryptoKey;
}

export interface GeneratedKey extends SigningKey {
  /** The 32-byte seed in hex, as `pipette-ctl` writes `id_ed25519`. Shown once, at sign-up. */
  seed: string;
  publicKey: string;
}

export const hex = (bytes: ArrayBuffer | Uint8Array): string =>
  Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, "0")).join("");

export function unhex(s: string): Uint8Array<ArrayBuffer> {
  if (!/^(?:[0-9a-fA-F]{2})*$/.test(s)) throw new Error("not hex");
  return Uint8Array.from(s.match(/../g) ?? [], (b) => parseInt(b, 16));
}

function base64url(s: string): Uint8Array<ArrayBuffer> {
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

/** `userkey_` + the hex sha256 of the public key: what the server registers it under. */
export async function keyId(publicKey: Uint8Array<ArrayBuffer>): Promise<string> {
  return "userkey_" + hex(await crypto.subtle.digest("SHA-256", publicKey));
}

function requireEd25519(): void {
  if (globalThis.isSecureContext === false || !crypto.subtle) {
    throw new Error("This page needs a secure context (https, or http on localhost) to sign.");
  }
}

async function fromPkcs8(pkcs8: Uint8Array<ArrayBuffer>): Promise<SigningKey & { publicKey: Uint8Array<ArrayBuffer> }> {
  // Extractable only long enough to read the public half off its JWK.
  const open = await crypto.subtle.importKey("pkcs8", pkcs8, ALG, true, ["sign"]);
  const jwk = await crypto.subtle.exportKey("jwk", open);
  if (!jwk.x) throw new Error("This browser does not expose the public half of an Ed25519 key.");
  const publicKey = base64url(jwk.x);
  const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, ALG, false, ["sign"]);
  return { key_id: await keyId(publicKey), privateKey, publicKey };
}

/** A pasted or picked key: the 64-hex seed `pipette-ctl` stores, or a whole PKCS#8 blob. */
export async function importKey(text: string): Promise<SigningKey> {
  requireEd25519();
  const t = text.trim();
  try {
    const { key_id, privateKey } = await fromPkcs8(unhex(t.length === 64 ? PKCS8_PREFIX + t : t));
    return { key_id, privateKey };
  } catch {
    throw new Error("That is not an Ed25519 private key. Expected the 64 hex characters in " +
                    ".pipette-ctl/identity/id_ed25519 (.pipette-plan/ in an older work directory).");
  }
}

export async function generateKey(): Promise<GeneratedKey> {
  requireEd25519();
  const pair = (await crypto.subtle.generateKey(ALG, true, ["sign", "verify"])) as CryptoKeyPair;
  const pkcs8 = new Uint8Array(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
  const { key_id, privateKey, publicKey } = await fromPkcs8(pkcs8);
  return { key_id, privateKey, publicKey: hex(publicKey), seed: hex(pkcs8).slice(PKCS8_PREFIX.length) };
}

// An ssh-ed25519 blob: the length-prefixed type name, then the 32-byte key.
const SSH_ED25519_PREFIX = "0000000b7373682d6564323535313900000020";

/**
 * A public key as the classic console's form took it:
 * 64 hex characters, or an `ssh-ed25519` line as `ssh-add -L` prints it.
 */
export function pastedPublicKey(raw: string): string {
  const [kind = "", blob = ""] = raw.trim().split(/\s+/);
  if (kind === "ssh-ed25519") {
    let bytes: string;
    try { bytes = hex(Uint8Array.from(atob(blob), (c) => c.charCodeAt(0))); } catch { bytes = ""; }
    const key = bytes.startsWith(SSH_ED25519_PREFIX) ? bytes.slice(SSH_ED25519_PREFIX.length) : "";
    if (key.length !== 64) throw new Error("that ssh-ed25519 line does not carry a key");
    return key;
  }
  if (kind.includes("-")) throw new Error(`a ${kind} key cannot sign here; only ssh-ed25519 keys can`);
  return raw.trim().toLowerCase();
}

/** `publicKey`'s (64 hex) `SHA256:` fingerprint, as `ssh-add -l` and `pipette-ctl auth keys available` print it. */
export async function sshFingerprint(publicKey: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", unhex(SSH_ED25519_PREFIX + publicKey)));
  return "SHA256:" + btoa(String.fromCharCode(...digest)).replace(/=+$/, "");
}

/** Hand `seed` to the reader as an `id_ed25519` file, the form `pipette-ctl` keeps a key in. */
export function downloadSeed(seed: string) {
  const link = document.createElement("a");
  link.href = URL.createObjectURL(new Blob([`${seed}\n`], { type: "application/octet-stream" }));
  link.download = "id_ed25519";
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href));
}

/** What this browser is, for the key list a revocation is decided from. */
export function describeBrowser(): string {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox"
    : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "a browser";
  const os = /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows"
    : /Android/.test(ua) ? "Android" : /iPhone|iPad/.test(ua) ? "iOS" : /Linux/.test(ua) ? "Linux" : "";
  return `console app, ${browser}${os ? ` on ${os}` : ""}`;
}
