import { createRawSnippet } from "svelte";
import { render } from "svelte/server";
import { describe, expect, test } from "vitest";
import CatalogName from "../src/lib/components/CatalogName.svelte";

const descriptor = createRawSnippet(() => ({ render: () => "<code>stored descriptor</code>" }));

describe("a runtime descriptor in a row", () => {
  test("shows its catalog name and member, with synonyms and a reveal button", () => {
    const { body } = render(CatalogName, {
      props: {
        name: {
          display: "llamacpp-v0.7.0::linux-cuda-12-8",
          name: "llamacpp-v0.7.0",
          synonyms: ["b11669::linux-cuda-12-8"],
        },
        children: descriptor,
      },
    });
    expect(body).toContain("llamacpp-v0.7.0");
    expect(body).toContain("::linux-cuda-12-8");
    expect(body).toContain("b11669::linux-cuda-12-8");
    expect(body).toContain('aria-expanded="false"');
    expect(body).not.toContain("stored descriptor");
  });

  test("opens its descriptor in a balloon that the name targets", () => {
    const { body } = render(CatalogName, {
      props: { name: { display: "llamacpp-v0.7.0", name: "llamacpp-v0.7.0", synonyms: [] }, children: descriptor },
    });
    const target = /popovertarget="([^"]+)"/.exec(body)?.[1];
    expect(target).toBeTruthy();
    expect(body).toMatch(new RegExp(`id="${target}" popover="auto"`));
  });

  test("shows an unnamed runtime descriptor as stored", () => {
    const { body } = render(CatalogName, { props: { name: null, children: descriptor } });
    expect(body).toContain("<code>stored descriptor</code>");
    expect(body).not.toContain("catalog-name");
    expect(body).not.toContain("popover");
  });
});
