import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import type { OwnUserView, UserKey } from "../src/lib/api-types";
import { importKey } from "../src/lib/keys";
import {
  adoptLegacy, dropLegacy, LEGACY_ENTRY, mustSaveFirst, parseLegacy, readLegacy, standing, type LegacyIdentity,
} from "../src/lib/legacy";

const SEED = "a".repeat(64);
const ENTRY = JSON.stringify({ user_id: "user_1", key_id: "userkey_x", private_key: SEED });

/** A `Storage` holding `entries`, which also refuses when told to. */
function storage(entries: Record<string, string>, refuses = false) {
  return {
    entries,
    getItem: (key: string) => {
      if (refuses) throw new Error("SecurityError");
      return entries[key] ?? null;
    },
    removeItem: (key: string) => {
      if (refuses) throw new Error("SecurityError");
      delete entries[key];
    },
  };
}

describe("the classic console's stored key", () => {
  test("reads the account and the seed, whatever else the entry holds", () => {
    expect(parseLegacy(ENTRY)).toEqual({ user_id: "user_1", private_key: SEED, generated: true });
  });

  test("a key the browser did not make says so; any other flag counts as made there", () => {
    const entry = (generated: unknown) => JSON.stringify({ user_id: "user_1", private_key: SEED, generated });
    expect(parseLegacy(entry(false))?.generated).toBe(false);
    for (const generated of [true, null, "false", 0]) {
      expect(parseLegacy(entry(generated))?.generated, String(generated)).toBe(true);
    }
  });

  test("only a key that may lock the account out waits for a save before signing in", () => {
    expect(mustSaveFirst("sole", false)).toBe(true);
    expect(mustSaveFirst("sole", true)).toBe(false);
    expect(mustSaveFirst("spare", false)).toBe(false);
  });

  describe("standing", () => {
    const made: LegacyIdentity = { user_id: "user_1", private_key: SEED, generated: true };
    const key = (key_id: string, expires_at?: string): UserKey =>
      ({ key_id, expires_at, public_key: "", key_details: "laptop", added_at: "2026-01-01T00:00:00Z" });
    /** An account holding the legacy key and `others`. */
    const holding = (...others: UserKey[]) => async ({ key_id }: { key_id: string }) =>
      ({ user_id: "user_1", keys: [key(key_id), ...others] }) as OwnUserView;
    const refusing = (e: unknown) => () => Promise.reject(e);

    test("a key another without an expiry backs up is spare; one backed only by expiring keys is sole", async () => {
      expect(await standing(made, holding(key("userkey_laptop")))).toBe("spare");
      expect(await standing(made, holding(key("userkey_browser", "2026-12-01T00:00:00Z")))).toBe("sole");
      expect(await standing(made, holding())).toBe("sole");
    });

    test("asks as the legacy key itself", async () => {
      const own = (await importKey(SEED)).key_id;
      const asked: string[] = [];
      await standing(made, async (signer) => {
        asked.push(signer.key_id);
        return holding()(signer);
      });
      expect(asked).toEqual([own]);
    });

    test("a key the server no longer holds is dead, whoever made it", async () => {
      for (const generated of [true, false]) {
        for (const reason of ["key not found", "key expired", "user not found"]) {
          expect(await standing({ ...made, generated }, refusing(new ApiError(401, reason)))).toBe("dead");
        }
      }
    });

    test("a 401 that does not say the key is gone keeps it", async () => {
      for (const reason of ["invalid signature", "timestamp expired", "signature already used"]) {
        expect(await standing(made, refusing(new ApiError(401, reason)))).toBe("sole");
        expect(await standing({ ...made, generated: false }, refusing(new ApiError(401, reason)))).toBe("spare");
      }
    });

    test("any other failure proves nothing: a key the browser made stays sole", async () => {
      for (const e of [new ApiError(503, "unavailable"), new TypeError("Failed to fetch")]) {
        expect(await standing(made, refusing(e))).toBe("sole");
        expect(await standing({ ...made, generated: false }, refusing(e))).toBe("spare");
      }
      expect(await standing({ ...made, private_key: "not a key" }, holding(key("userkey_laptop")))).toBe("sole");
    });

    test("a key from elsewhere is spare: its file is the reader's", async () => {
      expect(await standing({ ...made, generated: false }, holding())).toBe("spare");
    });
  });

  test("is dropped only once the sign-in with it succeeds", async () => {
    const held = storage({ [LEGACY_ENTRY]: ENTRY });
    const identity = readLegacy(held)!;
    await expect(adoptLegacy(identity, held, () => Promise.reject(new Error("401")))).rejects.toThrow("401");
    expect(readLegacy(held)).toEqual(identity);
    const signedIn: string[] = [];
    await expect(adoptLegacy(identity, held, async (key) => {
      expect(readLegacy(held), "still held while signing in").not.toBeNull();
      signedIn.push(key);
      return "identity";
    })).resolves.toBe("identity");
    expect(signedIn).toEqual([SEED]);
    expect(readLegacy(held)).toBeNull();
  });

  test("an absent, corrupt or partial entry is no key", () => {
    for (const raw of [null, "", "not json", "null", "[]", '"x"',
                       JSON.stringify({ user_id: "user_1" }),
                       JSON.stringify({ private_key: SEED }),
                       JSON.stringify({ user_id: "", private_key: SEED }),
                       JSON.stringify({ user_id: 1, private_key: SEED })]) {
      expect(parseLegacy(raw), String(raw)).toBeNull();
    }
  });

  test("is read from its entry and dropped from it", () => {
    const held = storage({ [LEGACY_ENTRY]: ENTRY, other: "kept" });
    expect(readLegacy(held)).toEqual({ user_id: "user_1", private_key: SEED, generated: true });
    dropLegacy(held);
    expect(held.entries).toEqual({ other: "kept" });
    expect(readLegacy(held)).toBeNull();
  });

  test("storage the browser refuses holds none and drops quietly", () => {
    const refused = storage({ [LEGACY_ENTRY]: ENTRY }, true);
    expect(readLegacy(refused)).toBeNull();
    expect(() => dropLegacy(refused)).not.toThrow();
    expect(readLegacy(null)).toBeNull();
    expect(() => dropLegacy(null)).not.toThrow();
  });
});
