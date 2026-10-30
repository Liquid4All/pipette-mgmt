import { describe, expect, test } from "vitest";
import { type Box, tailOf } from "../src/lib/balloon";

// A name 200px wide whose center is at x = 200, from y = 100 to y = 120.
const anchor: Box = { left: 100, right: 300, top: 100, bottom: 120 };
const box = (left: number, top: number, width: number, height: number): Box =>
  ({ left, right: left + width, top, bottom: top + height });

describe("a balloon's tail", () => {
  test.each([
    ["below the name, under its center", box(100, 128, 400, 80), { edge: "top", x: 100 }],
    ["above the name, over its center", box(100, 12, 400, 80), { edge: "bottom", x: 100 }],
    ["flipped left of the name, still under its center", box(-50, 128, 300, 80), { edge: "top", x: 250 }],
    ["left of the name's center, kept off the corner", box(130, 128, 40, 80), { edge: "top", x: 28 }],
    ["right of the name's center, kept off the corner", box(210, 128, 300, 80), { edge: "top", x: 12 }],
    ["far from the name, as in the viewport's center", box(300, 400, 400, 80), null],
    ["over the name", box(100, 110, 400, 80), null],
  ])("%s", (_, balloon, expected) => {
    expect(tailOf(anchor, balloon)).toEqual(expected);
  });
});
