import { describe, expect, test } from "vitest";
import { orgTrail } from "../src/lib/crumbs";

describe("an organization's crumbs", () => {
  test("link the organizations, then its overview by handle", () => {
    expect(orgTrail({ slug: "acme", org_id: "org_acme" }))
      .toEqual([{ label: "Organizations", href: "/app/orgs" }, { label: "acme", href: "/app/orgs/acme" }]);
    // The reserved slug is addressed by id, as the tree links it.
    expect(orgTrail({ slug: "new", org_id: "org_new" })[1]).toEqual({ label: "new", href: "/app/orgs/org_new" });
  });

  test("name the address's handle before it resolves", () => {
    expect(orgTrail("org_acme")[1]).toEqual({ label: "org_acme", href: "/app/orgs/org_acme" });
  });
});
