// The key the classic console last signed in with, which it kept in
// `localStorage`; one its sign-up made may be the only copy. The app offers to
// save it and either sign in with it (dropping the entry) or, already signed in, forget it.

import { ApiError } from "./api";
import type { OwnUserView } from "./api-types";
import { importKey, type SigningKey } from "./keys";

/** Where the classic console kept it. */
export const LEGACY_ENTRY = "pipette-mgmt.console.identity";

/**
 * The entry: the account, the key's 32-byte seed, hex, as `id_ed25519` holds
 * it, and whether that browser made the key, so the entry may be its only copy.
 */
export interface LegacyIdentity {
  user_id: string;
  private_key: string;
  generated: boolean;
}

/** The identity `raw` holds, or `null` for none or a corrupt entry, as the classic page read it. */
export function parseLegacy(raw: string | null): LegacyIdentity | null {
  try {
    const value: unknown = JSON.parse(raw ?? "null");
    if (typeof value !== "object" || value === null) return null;
    const { user_id, private_key, generated } = value as Record<string, unknown>;
    // An entry without the flag counts as made there, as the classic sign-out read it.
    return typeof user_id === "string" && user_id && typeof private_key === "string" && private_key
      ? { user_id, private_key, generated: generated !== false } : null;
  } catch {
    return null;
  }
}

/** This page's `localStorage`, or `null` where the browser refuses it, as it may with storage blocked. */
export function pageStorage(): Storage | null {
  try { return localStorage; } catch { return null; }
}

/** The identity `storage` holds; storage the browser refuses reads as none. */
export function readLegacy(storage: Pick<Storage, "getItem"> | null): LegacyIdentity | null {
  try {
    return parseLegacy(storage?.getItem(LEGACY_ENTRY) ?? null);
  } catch {
    return null;
  }
}

/** Forget it. */
export function dropLegacy(storage: Pick<Storage, "removeItem"> | null) {
  try { storage?.removeItem(LEGACY_ENTRY); } catch { /* nothing to drop */ }
}

/** Whether signing in with a key of `standing` waits for the reader to save it, as it may be the only copy. */
export const mustSaveFirst = (standing: Standing, saved: boolean) => standing === "sole" && !saved;

/**
 * Sign in with `held` through `signIn`, then drop the entry. A failed sign-in
 * keeps it; a download the page cannot see finish never drops it.
 */
export async function adoptLegacy<T>(held: LegacyIdentity, storage: Pick<Storage, "removeItem"> | null,
                                     signIn: (privateKey: string) => Promise<T>): Promise<T> {
  const signedIn = await signIn(held.private_key);
  dropLegacy(storage);
  return signedIn;
}

/**
 * What losing `held` would cost: `dead` no longer signs, `spare` leaves the
 * account another key without an expiry, and `sole` may lock it out.
 */
export type Standing = "dead" | "spare" | "sole";

/** The `401`s that prove a key gone; any other may be a skewed clock or a replayed request. */
const DEAD_KEY = new Set(["key not found", "key expired", "user not found"]);

/** `held`'s standing, asking `read` for the account it signs for; only a {@link DEAD_KEY} refusal proves it dead. */
export async function standing(held: LegacyIdentity, read: (key: SigningKey) => Promise<OwnUserView>): Promise<Standing> {
  try {
    const key = await importKey(held.private_key);
    const { keys } = await read(key);
    return !held.generated || keys.some((each) => each.key_id !== key.key_id && !each.expires_at) ? "spare" : "sole";
  } catch (e) {
    if (e instanceof ApiError && e.status === 401 && DEAD_KEY.has(e.reason)) return "dead";
    return held.generated ? "sole" : "spare";
  }
}
