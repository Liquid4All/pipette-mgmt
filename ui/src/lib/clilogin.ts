// The page `pipette-ctl auth login` opens (`/app/login/cli`): the key its link
// carries, the code that ties the page to the terminal, and the approval.

import { addKey, ApiError, type Signer } from "./api";
import { keyId, sshFingerprint, unhex } from "./keys";

/** What the link asks: a public key (64 lowercase hex) and what its machine says it is. */
export interface CliRequest {
  public_key: string;
  key_details: string;
}

/** The link's request, or `null` unless `key` is 64 hex and `details` is non-blank. */
export function cliRequest(query: URLSearchParams): CliRequest | null {
  const key = query.get("key") ?? "";
  const details = (query.get("details") ?? "").trim();
  if (!/^[0-9a-fA-F]{64}$/.test(key) || !details) return null;
  return { public_key: key.toLowerCase(), key_details: details };
}

/** How the page names the key; `code` must match `pipette-ctl auth login`'s `approval_code`. */
export interface KeyNames {
  key_id: string;
  fingerprint: string;
  code: string;
}

export async function keyNames(publicKey: string): Promise<KeyNames> {
  const key_id = await keyId(unhex(publicKey));
  const digest = key_id.slice("userkey_".length).toUpperCase();
  return { key_id, fingerprint: await sshFingerprint(publicKey), code: `${digest.slice(0, 4)}-${digest.slice(4, 8)}` };
}

const bare = (code: string) => code.replace(/[\s-]/g, "").toUpperCase();

/** Whether the reader typed `code`, case, spaces and `-` aside. */
export const codeMatches = (typed: string, code: string) => bare(typed) === bare(code);

/** Whether `typed` is as long as a whole code, so a mismatch is worth flagging. */
export const codeComplete = (typed: string) => bare(typed).length >= 8;

/** How long the approved key signs: as `auth register`'s does, or the 30 days the server allows at most. */
export type Expiry = "never" | "30d";

export const EXPIRY_DAYS = 30;

export const expiresAt = (expiry: Expiry, now = new Date()): string | undefined =>
  expiry === "never" ? undefined : new Date(now.getTime() + EXPIRY_DAYS * 86_400_000).toISOString();

/** What an approval came to; `taken` is a key another account or device holds. Other failures throw. */
export type Approval = "approved" | "taken";

export async function approve(signer: Signer, request: CliRequest, expiry: Expiry, now = new Date()): Promise<Approval> {
  try {
    await addKey(signer, { ...request, expires_at: expiresAt(expiry, now) });
    return "approved";
  } catch (e) {
    if (e instanceof ApiError && e.status === 409) return "taken";
    throw e;
  }
}
