// Signing in with a key the account already holds, or through Clerk: the browser
// ends up with an expiring key of its own, and keeps only that one.

import { addKey, registerWithClerk, revokeKey, whoami } from "./api";
import type { OwnUserView } from "./api-types";
import { CLERK_RETURN, clerkToken, dropOwedSignOut, type ClerkJs, type FlagStorage } from "./clerk";
import type { Identity } from "./identity";
import { generateKey, importKey, type SigningKey } from "./keys";

/** How long a key made for this browser signs, as Clerk's does; the server caps it at 30. */
export const BROWSER_KEY_DAYS = 7;

async function browserKey(key: SigningKey, user_id: string, key_details: string): Promise<Identity> {
  const fresh = await generateKey();
  const expires_at = new Date(Date.now() + BROWSER_KEY_DAYS * 86_400_000).toISOString();
  await addKey({ ...key, user_id }, { public_key: fresh.publicKey, key_details, expires_at });
  return { key_id: fresh.key_id, privateKey: fresh.privateKey, user_id, origin: "browser" };
}

/**
 * With the key `pipette-ctl auth keys add --expires-in` registered for this browser,
 * kept as it is. One added without an expiry, by a `pipette-ctl` (or `pipette-plan`)
 * older than the flag, would never expire, so it goes once an expiring one is in.
 */
export async function signInWithAddedKey(added: SigningKey, key_details: string): Promise<Identity> {
  let account: OwnUserView;
  try {
    account = await whoami(added);
  } catch (e) {
    throw new Error("This browser's key is not on an account yet. Run the command, then try again." +
                    (e instanceof Error ? ` (${e.message})` : ""));
  }
  const { user_id } = account;
  if (account.keys.find((key) => key.key_id === added.key_id)?.expires_at) {
    return { key_id: added.key_id, privateKey: added.privateKey, user_id, origin: "browser" };
  }
  const identity = await browserKey(added, user_id, key_details);
  await revokeKey(identity, added.key_id);
  return identity;
}

/** With the reader's own private key, which stays registered and is not kept here. */
export async function signInWithOwnKey(secret: string, key_details: string): Promise<Identity> {
  const own = await importKey(secret);
  const { user_id } = await whoami(own);
  return browserKey(own, user_id, key_details);
}

/**
 * `null` when the reader is sent to Clerk's page first. Drops an owed sign-out,
 * or the next load would end the session this one starts.
 */
export async function signInWithClerk(clerk: ClerkJs, key_details: string, back: string,
                                      storage: FlagStorage | null): Promise<Identity | null> {
  dropOwedSignOut(storage);
  if (!clerk.isSignedIn) {
    const url = new URL(back);
    url.searchParams.set(CLERK_RETURN, "1");
    await clerk.redirectToSignIn({ signInForceRedirectUrl: url.toString(), signUpForceRedirectUrl: url.toString() });
    return null;
  }
  const key = await generateKey();
  const { user_id } = await registerWithClerk(await clerkToken(clerk), { public_key: key.publicKey, key_details });
  return { key_id: key.key_id, privateKey: key.privateKey, user_id, origin: "clerk" };
}
