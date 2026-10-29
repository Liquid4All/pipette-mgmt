// Clerk sign-in for the app, offered only where the server stamped its settings
// into the page (`src/ui.rs`, at a host `[clerk] authorized_parties` names).
// ClerkJS loads only once a reader asks for it, or on a signed-out load that owes a failed sign-out.

import { describe } from "./components/guard";

interface ClerkSettings {
  publishable_key: string;
  frontend_api: string;
  template: string;
}

/** The slice of ClerkJS this app calls. */
export interface ClerkJs {
  isSignedIn: boolean;
  load(): Promise<void>;
  session?: { getToken(options: { template: string; skipCache: boolean }): Promise<string | null> };
  redirectToSignIn(options: { signInForceRedirectUrl: string; signUpForceRedirectUrl: string }): Promise<unknown>;
  signOut(options?: { redirectUrl?: string }): Promise<void>;
}

declare global {
  interface Window { Clerk?: ClerkJs }
}

/** Marks the address Clerk's page sends the reader back to. */
export const CLERK_RETURN = "clerk";

/** The address once a Clerk return is finished. */
export function returned(href: string): string {
  const url = new URL(href);
  url.searchParams.delete(CLERK_RETURN);
  return url.pathname + url.search + url.hash;
}

/**
 * Set until a Clerk sign-out went through, so the next signed-out load retries
 * a failed one. The classic console's flag, so a sign-out it owed is retried too.
 */
export const SIGNOUT_OWED = "pipette-mgmt.console.clerk-signout";

export type FlagStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** Whether `storage` holds a Clerk sign-out a load should retry; storage the browser refuses holds none. */
export function owesSignOut(storage: FlagStorage | null): boolean {
  try { return (storage?.getItem(SIGNOUT_OWED) ?? null) !== null; } catch { return false; }
}

/** Drop an owed sign-out: a Clerk sign-in the reader asked for replaces it. */
export function dropOwedSignOut(storage: FlagStorage | null) {
  try { storage?.removeItem(SIGNOUT_OWED); } catch { /* nothing to drop */ }
}

/** Owe a Clerk sign-out until `endClerkSession` drops it. */
export function oweSignOut(storage: FlagStorage | null) {
  try { storage?.setItem(SIGNOUT_OWED, "1"); } catch { /* the retry is lost, not the sign-out */ }
}

/** The owed flag is set first and dropped only on success, so a failed sign-out is retried next load. */
export async function endClerkSession(clerk: () => Promise<ClerkJs>, storage: FlagStorage | null,
                                      redirectUrl: string): Promise<void> {
  oweSignOut(storage);
  const loaded = await clerk();
  if (loaded.isSignedIn) await loaded.signOut({ redirectUrl });
  dropOwedSignOut(storage);
}

/** What a failed Clerk sign-out tells the reader, in the classic console's words. */
export const signOutFailed = (e: unknown): string =>
  `${describe(e)} You are signed out here, but this browser may still be signed in to Clerk. ` +
  "Reload this page to try again.";

// No document under vitest, which reads it as no Clerk.
export const settings: ClerkSettings | null = (() => {
  const raw = globalThis.document?.querySelector<HTMLMetaElement>('meta[name="pipette-clerk"]')?.content ?? "";
  if (!raw.startsWith("{")) return null;
  try { return JSON.parse(raw) as ClerkSettings; } catch { return null; }
})();

let loading: Promise<ClerkJs> | null = null;

export function loadClerk(): Promise<ClerkJs> {
  const config = settings;
  if (!config) return Promise.reject(new Error("This server offers no Clerk sign-in here."));
  loading ??= new Promise<ClerkJs>((resolve, reject) => {
    if (window.Clerk) return resolve(window.Clerk);
    const tag = document.createElement("script");
    tag.src = `https://${config.frontend_api}/npm/@clerk/clerk-js@6/dist/clerk.browser.js`;
    tag.crossOrigin = "anonymous";
    tag.setAttribute("data-clerk-publishable-key", config.publishable_key);
    tag.onload = () => (window.Clerk ? resolve(window.Clerk) : reject(new Error("ClerkJS did not start.")));
    tag.onerror = () => reject(new Error(`Clerk's script did not load from ${config.frontend_api}.`));
    document.head.appendChild(tag);
  }).then(async (clerk) => { await clerk.load(); return clerk; });
  loading.catch(() => { loading = null; });
  return loading;
}

/** A fresh token from the JWT template; the server spends each one once. */
export async function clerkToken(clerk: ClerkJs): Promise<string> {
  const template = settings?.template ?? "";
  const token = await clerk.session?.getToken({ template, skipCache: true });
  if (!token) throw new Error(`Clerk issued no token. Its ${template} JWT template may be missing.`);
  return token;
}
