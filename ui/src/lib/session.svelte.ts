// Who is signed in, and what the frame draws for them: the account as
// `GET /api/v1/users/me` resolves it, and the other organizations the tree lists.

import { ApiError, me, onUnauthorized, org, orgs, revokeKey } from "./api";
import {
  CLERK_RETURN, endClerkSession, loadClerk, oweSignOut, owesSignOut, settings as clerkSettings, signOutFailed,
} from "./clerk";
import { describe } from "./components/guard";
import { handle } from "./format";
import { browserOnly, clear, load, save, type Identity } from "./identity";
import { describeBrowser } from "./keys";
import { pageStorage } from "./legacy";
import { bySlug, knownOrg, ownOrg, resolveOrg, type OrgRef } from "./orgs";
import { signInWithClerk } from "./signin";
import type { OrgView, OwnUserView } from "./api-types";

const LAPSED = "This browser's key no longer signs in: it expired or was revoked.";

const ERASES_GENERATED = "Signing out erases the key this browser generated. Without the id_ed25519 you " +
  "saved at sign-up, only an operator can let you back in.\n\nSign out anyway?";

class Session {
  identity = $state<Identity | null>(null);
  account = $state<OwnUserView | null>(null);
  /** Every organization but the reader's own, by handle, for a holder of `read_any_orgs`. */
  others = $state<OrgView[]>([]);
  ready = $state(false);
  error = $state("");
  /** Organizations looked up for `#lookedFor`, by the reference asked, id and handle; `null` for none. */
  #looked = new Map<string, OrgRef | OrgView | null>();
  /** Lookups under way, by the reference asked. */
  #running = new Map<string, Promise<OrgRef | OrgView | null>>();
  #lookedFor: Identity | null = null;

  /** Whether `error` is a `401` on the signed-in key, which a Clerk session may replace. */
  get lapsedKey() { return this.error === LAPSED; }

  /** The key every page signs with; a page draws only while one is signed in. */
  get signer(): Identity {
    if (!this.identity) throw new Error("This browser is not signed in.");
    return this.identity;
  }

  /** Sign in with the key this browser kept, if it kept one; signed out, retry a Clerk sign-out a load owes. */
  start() {
    load()
      .then(async (stored) => {
        if (stored) return this.signedIn(stored);
        const storage = pageStorage();
        // Back from Clerk's page, the session is the one the reader just asked for.
        const returning = new URLSearchParams(location.search).has(CLERK_RETURN);
        if (!clerkSettings || returning || !owesSignOut(storage)) return;
        // Not awaited: the sign-in page draws without waiting on Clerk's host.
        void endClerkSession(loadClerk, storage, location.href)
          .catch((e) => { if (!this.identity) this.error = signOutFailed(e); });
      })
      .catch((e) => (this.error = `This browser's key store did not open: ${e}`))
      .finally(() => (this.ready = true));
  }

  /** Read the account and the organizations the tree lists. */
  async refresh() {
    const signer = this.signer;
    this.#forget();
    // An answer for a key signed out meanwhile is dropped.
    try {
      const read = await me(signer);
      const others = read.may.read_roster === "every" ? await orgs(signer, { counts: false }).catch(() => []) : [];
      if (this.identity !== signer) return;
      this.others = others
        .filter((each) => each.org_id !== read.org_id)
        .sort(bySlug);
      this.account = read;
      this.error = "";
    } catch (e) {
      if (this.identity !== signer) return;
      this.error = e instanceof ApiError && e.status === 401 ? LAPSED : describe(e);
    }
  }

  async signedIn(next: Identity) {
    this.identity = next;
    await this.refresh();
  }

  async signOut() {
    const leaving = this.identity;
    const endsClerk = leaving?.origin === "clerk" && clerkSettings;
    // Owed before the key goes, so a sign-out cut short here is still retried.
    if (endsClerk) oweSignOut(pageStorage());
    // A key made for this browser alone goes with it; the reader's own key stays.
    if (leaving && browserOnly(leaving.origin)) {
      await revokeKey(leaving, leaving.key_id).catch(() => {});
    }
    await this.#forgetKey();
    // Or the next person at this browser is one click from the account; the next signed-out load retries a failure.
    if (endsClerk) {
      await endClerkSession(loadClerk, pageStorage(), "/app/").catch((e) => { this.error = signOutFailed(e); });
    }
  }

  /** `signOut`, once the reader agreed to erase a key this browser generated at sign-up. */
  async leave() {
    if (this.identity?.origin === "generated" && !confirm(ERASES_GENERATED)) return;
    await this.signOut();
  }

  /**
   * A lapsed Clerk key's way back: a new key while Clerk's session lives, or
   * Clerk's page first, without signing out of Clerk. Only that way forgets the
   * lapsed key first, so the return lands signed out, where the sign-in finishes.
   */
  async renewWithClerk() {
    try {
      const clerk = await loadClerk();
      if (!clerk.isSignedIn) await this.#forgetKey();
      const identity = await signInWithClerk(clerk, describeBrowser(), location.href, pageStorage());
      if (!identity) return;
      await save(identity);
      await this.signedIn(identity);
    } catch (e) {
      this.error = describe(e);
    }
  }

  /** Forget the signed-in key here, and everything read with it. */
  async #forgetKey() {
    await clear();
    this.identity = this.account = null;
    this.others = [];
    this.#forget();
    this.error = "";
  }

  /** A `401` on the signed-in key: it lapsed, so every page gives way to saying so. */
  lapsed(signer: { key_id: string }) {
    if (signer.key_id !== this.identity?.key_id) return;
    this.error = LAPSED;
  }

  /** The organizations the reader holds already: its own, and the others the tree lists. */
  get known(): (OrgRef | OrgView | null)[] {
    const account = this.account;
    return [account ? ownOrg(account) : null, ...this.others];
  }

  /** The organization `reference` (a handle or an id) names, as the reader may read it; `null` when none does. */
  knownOrg(reference: string): OrgRef | OrgView | null {
    const looked = this.#lookedFor === this.identity ? this.#looked.get(reference) : null;
    return knownOrg(reference, this.known) ?? looked ?? null;
  }

  /** Drop the lookups: what the reader may read, or what exists, may have changed. */
  #forget() {
    this.#looked.clear();
    this.#running.clear();
    this.#lookedFor = this.identity;
  }

  /**
   * Like `knownOrg`, asking `GET /api/v1/orgs/{reference}` for one the reader does not
   * hold or has not looked up yet, or with `fresh` for every one, for a page that
   * shows its counts. One read answers every lookup of a reference while it runs.
   */
  async resolveOrg(reference: string, fresh = false): Promise<OrgRef | OrgView | null> {
    const signer = this.identity;
    if (!this.account || !signer) return null;
    if (this.#lookedFor !== signer) this.#forget();
    const held = knownOrg(reference, this.known);
    if (!fresh) {
      if (held) return held;
      if (this.#looked.has(reference)) return this.#looked.get(reference) ?? null;
    }
    // A read under way is as fresh as a new one.
    let read = this.#running.get(reference);
    if (!read) {
      const started = resolveOrg(reference, held ? [held] : [], (each) => org(signer, each), true);
      read = started;
      this.#running.set(reference, started);
      started
        .then((found) => {
          if (this.#lookedFor !== signer) return;
          this.#looked.set(reference, found);
          if (found) for (const key of [found.org_id, handle(found)]) this.#looked.set(key, found);
        })
        .catch(() => {})
        .finally(() => { if (this.#running.get(reference) === started) this.#running.delete(reference); });
    }
    const found = await read;
    return this.identity === signer ? found : null;
  }
}

export const session = new Session();

onUnauthorized((signer) => session.lapsed(signer));
