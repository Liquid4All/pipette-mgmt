import { afterEach, describe, expect, test, vi } from "vitest";
import { SIGNOUT_OWED, endClerkSession, owesSignOut, returned, signOutFailed, type ClerkJs } from "../src/lib/clerk";
import { signInWithClerk } from "../src/lib/signin";

/** `localStorage`'s slice the flag takes, in memory. */
class Flags {
  readonly held: Map<string, string>;
  constructor(owed = false) { this.held = new Map(owed ? [[SIGNOUT_OWED, "1"]] : []); }
  getItem(key: string) { return this.held.get(key) ?? null; }
  setItem(key: string, value: string) { this.held.set(key, value); }
  removeItem(key: string) { this.held.delete(key); }
}

/** ClerkJS as far as the app calls it, recording what it was asked. */
function fakeClerk(signedIn: boolean, fails = false) {
  const asked: { signOut: (string | undefined)[]; redirect: string[] } = { signOut: [], redirect: [] };
  const clerk: ClerkJs = {
    isSignedIn: signedIn,
    load: async () => {},
    redirectToSignIn: async ({ signInForceRedirectUrl }) => { asked.redirect.push(signInForceRedirectUrl); },
    signOut: async (options) => {
      asked.signOut.push(options?.redirectUrl);
      if (fails) throw new Error("Clerk is unreachable.");
    },
  };
  return { clerk, asked };
}

describe("the return from Clerk's page", () => {
  test("drops only the marker, keeping the page's query and anchor", () => {
    expect(returned("https://mgmt.example/app/plans?status=active&clerk=1#job-1")).toBe("/app/plans?status=active#job-1");
    expect(returned("https://mgmt.example/app/?clerk=1")).toBe("/app/");
    expect(returned("https://mgmt.example/app/vault?clerk=1&owner=mine")).toBe("/app/vault?owner=mine");
  });

  test("a reader with no Clerk session is sent to Clerk's page, coming back to this one marked", async () => {
    const { clerk, asked } = fakeClerk(false);
    const flags = new Flags(true);
    expect(await signInWithClerk(clerk, "console app", "https://mgmt.example/app/plans?status=active#job-1", flags))
      .toBeNull();
    expect(asked.redirect).toEqual(["https://mgmt.example/app/plans?status=active&clerk=1#job-1"]);
    // Owed still, the return would end the session Clerk's page just made.
    expect(owesSignOut(flags)).toBe(false);
  });
});

describe("a Clerk sign-in with a live session", () => {
  afterEach(() => vi.unstubAllGlobals());

  test("registers a key and drops an owed sign-out, which would end that session next load", async () => {
    const asked: string[] = [];
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      asked.push(`${init.method} ${path} ${(init.headers as Record<string, string>).Authorization}`);
      return new Response(JSON.stringify({ user_id: "user_1", status: "active", key_id: "k" }), { status: 201 });
    }));
    const flags = new Flags(true);
    const { clerk } = fakeClerk(true);
    clerk.session = { getToken: async () => "clerk-token" };
    const identity = await signInWithClerk(clerk, "console app", "https://mgmt.example/app/", flags);
    expect(identity).toMatchObject({ user_id: "user_1", origin: "clerk" });
    expect(asked).toEqual(["POST /api/v1/users/register/clerk Bearer clerk-token"]);
    expect(owesSignOut(flags)).toBe(false);
  });
});

describe("signing out of Clerk", () => {
  test("clears the flag once Clerk's session ended", async () => {
    const flags = new Flags();
    const { clerk, asked } = fakeClerk(true);
    await endClerkSession(async () => clerk, flags, "/app/");
    expect(asked.signOut).toEqual(["/app/"]);
    expect(owesSignOut(flags)).toBe(false);
  });

  test("keeps the flag when it failed, so the next load retries it", async () => {
    const flags = new Flags();
    const { clerk } = fakeClerk(true, true);
    await expect(endClerkSession(async () => clerk, flags, "/app/")).rejects.toThrow("Clerk is unreachable.");
    expect(flags.getItem(SIGNOUT_OWED)).toBe("1");
    expect(owesSignOut(flags)).toBe(true);
    await expect(endClerkSession(() => Promise.reject(new Error("no script")), flags, "/app/")).rejects.toThrow();
    expect(owesSignOut(flags)).toBe(true);
  });

  test("a retry with no Clerk session left only clears the flag", async () => {
    const flags = new Flags(true);
    const { clerk, asked } = fakeClerk(false);
    await endClerkSession(async () => clerk, flags, "https://mgmt.example/app/plans");
    expect(asked.signOut).toEqual([]);
    expect(owesSignOut(flags)).toBe(false);
  });

  test("storage the browser refuses owes nothing, and the failure is worded for the reader", () => {
    expect(owesSignOut(null)).toBe(false);
    const blocked = () => { throw new Error("blocked"); };
    expect(owesSignOut({ getItem: blocked, setItem: blocked, removeItem: blocked })).toBe(false);
    expect(signOutFailed(new Error("Clerk is unreachable."))).toBe(
      "Clerk is unreachable. You are signed out here, but this browser may still be signed in to Clerk. " +
      "Reload this page to try again.");
  });
});
