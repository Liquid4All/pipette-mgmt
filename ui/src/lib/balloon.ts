// Where a balloon's tail goes, so that it points at the element that opened the balloon.

/** A box in viewport pixels, as `getBoundingClientRect` gives it. */
export interface Box {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

/** The balloon edge that holds the tail, and the tail's distance from the balloon's left edge. */
export interface Tail {
  edge: "top" | "bottom";
  x: number;
}

/** The largest gap between a balloon and its anchor that still gets a tail. */
const MAX_GAP = 16;
/** The closest that the tail comes to a corner of the balloon. */
const INSET = 12;

/**
 * The tail of a balloon at `balloon` for an anchor at `anchor`: on the edge that faces the anchor, across from the
 * anchor's center. Null where the balloon is not next to the anchor, such as a balloon in the center of the viewport.
 */
export function tailOf(anchor: Box, balloon: Box): Tail | null {
  const edge = balloon.top >= anchor.bottom && balloon.top - anchor.bottom <= MAX_GAP ? "top"
    : balloon.bottom <= anchor.top && anchor.top - balloon.bottom <= MAX_GAP ? "bottom"
    : null;
  if (!edge) return null;
  const center = (anchor.left + anchor.right) / 2 - balloon.left;
  return { edge, x: Math.min(Math.max(center, INSET), balloon.right - balloon.left - INSET) };
}
