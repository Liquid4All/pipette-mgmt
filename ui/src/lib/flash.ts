// A refusal carried to the page a write moved to, as the classic console drew
// a refused step on the page it lands on. Held in memory only, for one page.

let carried: { path: string; message: string } | null = null;

/** Leave `message` for the page at `path` to show. */
export function carry(path: string, message: string) {
  carried = { path, message };
}

/** The message left for `path`, once; any other page's is dropped. */
export function take(path: string): string {
  const found = carried;
  carried = null;
  return found?.path === path ? found.message : "";
}
