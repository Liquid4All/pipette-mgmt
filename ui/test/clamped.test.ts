import { render } from "svelte/server";
import { describe, expect, test } from "vitest";
import Clamped from "../src/lib/components/Clamped.svelte";

describe("a note in a table", () => {
  test("is held in its clamped box with its newlines", () => {
    const { body } = render(Clamped, { props: { text: "first line\nsecond line", note: true } });
    expect(body).toMatch(/class="text[^"]* note[^"]*"[^>]*>first line\nsecond line<\/span>/);
  });

  test("a line that is no note stays as it was", () => {
    const { body } = render(Clamped, { props: { text: "a warning" } });
    expect(body).not.toMatch(/class="[^"]*note/);
  });
});
