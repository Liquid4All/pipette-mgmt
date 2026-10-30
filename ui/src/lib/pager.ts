// A listing's pager, as the classic console's `Pager` and `pager.html`
// draw it.

export interface OffsetPager {
  /** The offsets the Previous and Next links open, where there is one. */
  previous: number | null;
  next: number | null;
  /** The rows this page covers, as `first–last of total`; `null` when it drew none. */
  range: string | null;
}

/**
 * The pager over `shown` rows at `offset`, pages of `size`. With a `total` the
 * range counts the listing; a filtered one passes `more` instead and has none.
 */
export function offsetPager(offset: number, shown: number, size: number,
                            total: number | null, more = false): OffsetPager {
  const last = offset + shown;
  return {
    previous: offset > 0 ? Math.max(0, offset - size) : null,
    next: (total === null ? more : last < total) ? last : null,
    range: shown ? `${offset + 1}–${last}${total === null ? "" : ` of ${total}`}` : null,
  };
}

/**
 * Default and maximum rows on a page the app pages itself, as the classic
 * roster and benchmark listings bound them.
 */
const PAGE_DEFAULT = 50;
const PAGE_MAX = 200;

/** The page the address asks for, within `PAGE_MAX`; `asked` is the `limit` as given, which links keep. */
export function offsetPaging(query: URLSearchParams): { offset: number; limit: number; asked: string } {
  const offset = Math.max(0, Number.parseInt(query.get("offset") ?? "", 10) || 0);
  const asked = query.get("limit") ?? "";
  const limit = Math.min(PAGE_MAX, Math.max(1, Number.parseInt(asked, 10) || PAGE_DEFAULT));
  return { offset, limit, asked };
}

/**
 * The listing's first page without `limit`, after a 400 under one; `null` otherwise.
 * Any 400 under `limit` offers it, whatever the cause; dropping `cursor` cures a stale one too.
 */
export function unsized(path: string, query: URLSearchParams, status: number | null): string | null {
  if (status !== 400 || !query.get("limit")) return null;
  const kept = new URLSearchParams([...query].filter(([key]) => key !== "limit" && key !== "cursor"));
  const search = kept.toString();
  return search ? `${path}?${search}` : path;
}
