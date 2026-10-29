// What every page's reads and writes share, kept free of runes for vitest.

/** An error as a page prints it. */
export const describe = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/**
 * A counter of starts: each start answers whether it is still the latest, so a
 * slow read that an address change overtook never draws over the newer one.
 */
export function latest(): () => () => boolean {
  let started = 0;
  return () => {
    const mine = ++started;
    return () => mine === started;
  };
}
