import { render } from "svelte/server";
import { describe, expect, test } from "vitest";
import OwnerCell from "../src/lib/components/OwnerCell.svelte";

describe("an owner cell", () => {
  test("names an organization by its display name and links it by id", () => {
    const { body } = render(OwnerCell, {
      props: { owner: { type: "org", org_id: "org_acme" }, names: { org_acme: "Acme Corp" } },
    });
    expect(body).toMatch(/href="\/app\/orgs\/org_acme"[^>]*>Acme Corp<\/a>/);
  });

  test("shows the id of an organization the body does not name", () => {
    const { body } = render(OwnerCell, { props: { owner: { type: "org", org_id: "org_acme" } } });
    expect(body).toMatch(/<a href="\/app\/orgs\/org_acme" class="id">org_acme<\/a>/);
  });
});
