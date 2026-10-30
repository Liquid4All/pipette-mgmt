import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import { absentOrg, createBody, createOrgRefusal, knownOrg, names, renameBody, resolveOrg } from "../src/lib/orgs";
import type { OwnUserView, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

const own = { org_id: "org_own", slug: "acme" };
const other = { org_id: "org_other", slug: "beta" };
const odd = { org_id: "org_new", slug: "new" };

/** A `GET /api/v1/orgs/{reference}` that answers `found`, or fails with `status`. */
const fetching = (found: typeof own | null, status = 404) => {
  const asked: string[] = [];
  const fetch = async (reference: string) => {
    asked.push(reference);
    if (!found) throw new ApiError(status, "refused");
    return found;
  };
  return { asked, fetch };
};

describe("orgs", () => {
  test("an organization is named by its handle or its id", () => {
    expect(names(own, "acme")).toBe(true);
    expect(names(own, "org_own")).toBe(true);
    expect(names(own, "beta")).toBe(false);
    // `render::handle` addresses a slug that reads as `new` by its id alone.
    expect(names(odd, "new")).toBe(false);
    expect(names(odd, "org_new")).toBe(true);
  });

  test("one the reader holds resolves without asking", async () => {
    const { asked, fetch } = fetching(null);
    expect(knownOrg("beta", [null, other])).toEqual(other);
    expect(await resolveOrg("acme", [own, other], fetch)).toEqual(own);
    expect(await resolveOrg("org_other", [own, other], fetch)).toEqual(other);
    expect(asked).toEqual([]);
  });

  test("one it does not hold is asked for, and a 404 is none", async () => {
    const away = { org_id: "org_away", slug: "gamma" };
    const answered = fetching(away);
    expect(await resolveOrg("gamma", [own], answered.fetch)).toEqual(away);
    expect(answered.asked).toEqual(["gamma"]);
    const refused = fetching(null);
    expect(await resolveOrg("org_gone", [own, null], refused.fetch)).toBeNull();
    expect(refused.asked).toEqual(["org_gone"]);
  });

  test("any other failure is not a missing organization", async () => {
    const forbidden = fetching(null, 403);
    await expect(resolveOrg("gamma", [own], forbidden.fetch)).rejects.toThrow("403: refused");
    const broken = fetching(null, 500);
    await expect(resolveOrg("gamma", [own], broken.fetch)).rejects.toThrow("500: refused");
  });

  test("fresh asks again for one the reader holds, by its id", async () => {
    const reread = { ...own, client_count: 3 };
    const { asked, fetch } = fetching(reread);
    expect(await resolveOrg("acme", [own], fetch, true)).toEqual(reread);
    expect(asked).toEqual(["org_own"]);
  });

  test("a refusal reads as none only for an organization the reader does not hold", () => {
    expect(absentOrg(new ApiError(404, "organization not found"), true)).toBe(true);
    expect(absentOrg(new ApiError(403, "user lacks the read_orgs permission"), false)).toBe(true);
    // Its own organization exists, so the refusal is of a grant and is said so.
    expect(absentOrg(new ApiError(403, "user lacks the read_orgs permission"), true)).toBe(false);
    expect(absentOrg(new ApiError(500, "store"), false)).toBe(false);
    expect(absentOrg(new Error("offline"), false)).toBe(false);
  });
});

const reader = (permissions: UserPermission[]) =>
  ({ permissions, may: abilitiesOf(permissions) } as unknown as OwnUserView);

describe("the organization forms", () => {
  test("a rename sends the name alone, and a blank name is refused as the classic form refuses it", () => {
    expect(renameBody("  Acme Holdings ")).toEqual({ display_name: "Acme Holdings" });
    expect(() => renameBody("   ")).toThrow("Name the organization");
  });

  test("a blank display name is left out, so the route names it after its handle", () => {
    expect(createBody(" acme ", "  ")).toEqual({ slug: "acme" });
    expect(createBody("acme", " Acme Corp. ")).toEqual({ slug: "acme", display_name: "Acme Corp." });
  });

  test("a handle OrgSlug refuses is refused as the classic form words it", () => {
    expect(createBody(" PR6-Upper ", "")).toEqual({ slug: "pr6-upper" });
    expect(() => createBody("Bad Handle", "")).toThrow(
      `invalid handle: organization slug "bad handle" contains invalid character ' '; allowed: [a-z0-9_-] (no '/')`);
    expect(() => createBody("org_x", "")).toThrow(
      `invalid handle: organization slug "org_x" may not start with "org_", which is how an organization id is spelled`);
    expect(() => createBody("a".repeat(65), "")).toThrow(`is too long (65 chars, max 64)`);
    expect(() => createBody("   ", "Acme")).toThrow("invalid handle: organization slug must not be empty");
    expect(createBody("a".repeat(64), "")).toEqual({ slug: "a".repeat(64) });
  });

  test("the create form is manage_any_orgs's", () => {
    expect(createOrgRefusal(reader(["manage_any_orgs", "manage_orgs"]))).toBe("");
    expect(createOrgRefusal(reader(["manage_orgs"]))).toBe("user lacks the manage_any_orgs permission");
  });
});
