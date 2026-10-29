// The page the address bar names. The server answers every path under `/app/`
// with this app, which draws the page itself, so a page is a bookmark and the
// back button walks the app's history. The patterns are `routes.ts`'s.

import { tick } from "svelte";
import { inApp, match, screenOf } from "./routes";

export { href } from "./routes";

/** The id the address's hash names; a malformed one names none. */
function hashed(): string {
  try { return decodeURIComponent(location.hash.slice(1)); } catch { return ""; }
}

class Here {
  path = $state(location.pathname);
  query = $state(new URLSearchParams(location.search));
  /** The id the hash names, which a page may draw differently for; a step to it alone reloads nothing. */
  hash = $state(hashed());
  route = $derived(match(this.path));
  /** The listing a record's page sits under, whose leaf the tree marks; a page sets it once it knows. */
  under = $state<string | null>(null);

  /** Follow the address bar; only what changed is reassigned, so a step to an anchor reloads nothing. */
  sync(): boolean {
    const query = new URLSearchParams(location.search);
    const moved = this.path !== location.pathname;
    const asked = this.query.toString() !== query.toString();
    if (moved) {
      this.path = location.pathname;
      this.under = null;
    }
    if (asked) this.query = query;
    this.hash = hashed();
    return moved || asked;
  }
}

export const here = new Here();

/** The element the address's hash names, scrolled to once its page has drawn it. */
let anchor = hashed();
/** Whether the anchor takes the focus too, as the link that led there is gone or far above it. */
let focusing = false;

// A load drawn above the anchor moves it, so it is followed until the reader takes over.
for (const taken of ["wheel", "touchstart", "keydown", "mousedown"]) {
  addEventListener(taken, () => { anchor = ""; }, { capture: true, passive: true });
}

/**
 * Scroll to the address's anchor once drawn, focusing it after a step to another
 * page or a followed link; a page calls it after each load.
 */
export async function reveal() {
  if (!anchor) return;
  await tick();
  const target = anchor ? document.getElementById(anchor) : null;
  if (!target) return;
  target.scrollIntoView();
  if (focusing && document.activeElement !== target) focusOn(target);
}

/** Where `land` puts the focus: the page's heading, else the page. */
const landing = (): HTMLElement | null =>
  document.querySelector<HTMLElement>("main h1") ?? document.querySelector<HTMLElement>("main");

/** Focus `target`, which need not be a control, leaving the scroll where it is. */
function focusOn(target: HTMLElement) {
  if (!target.hasAttribute("tabindex")) target.setAttribute("tabindex", "-1");
  target.focus({ preventScroll: true });
}

/** Focus the page just drawn, so a screen reader announces it as it would a page load. */
export async function land() {
  await tick();
  const target = landing();
  if (target) focusOn(target);
}

/** After a step between one screen's tabs: the focus stays, or goes to the open tab where the step removed it. */
async function stay() {
  await tick();
  if (document.activeElement && document.activeElement !== document.body) return;
  const open = document.querySelectorAll<HTMLElement>('main nav [aria-current="page"]');
  open[open.length - 1]?.focus({ preventScroll: true });
}

/** A step from `from` to another tab of the screen it was on. */
const tabbed = (from: string) => here.path !== from && screenOf(here.path) === screenOf(from);

/**
 * After a step to `here`: a new page takes the focus, its heading at once and
 * the anchored element once drawn; a followed link to an anchor on this page
 * gives it the focus too, but a filter applied on it, or another tab, leaves the focus be.
 */
function arrived(from: string, linked = false) {
  const moved = here.path !== from;
  focusing = (moved || linked) && anchor !== "";
  if (tabbed(from)) stay();
  else if (moved) land();
  if (anchor) reveal();
}

// The browser scrolls a step within the page itself; a page drawn anew reveals its anchor.
addEventListener("popstate", () => {
  const from = here.path;
  anchor = here.sync() ? hashed() : "";
  arrived(from);
});

/** Open `href` in place, as a new history entry. */
export function navigate(href: string) {
  const from = here.path;
  history.pushState(null, "", href);
  here.sync();
  anchor = hashed();
  // Another tab keeps the reader's place, so the bar stays where it was clicked.
  if (!anchor && !tabbed(from)) scrollTo(0, 0);
  arrived(from, true);
}

/** Open `href` in place of the current entry, as a server redirect would. */
export function redirect(href: string) {
  // The page is drawn anew, so a focus `land` gave the old one moves to the new one.
  const landed = document.activeElement !== null && document.activeElement === landing();
  const from = here.path;
  history.replaceState(null, "", href);
  here.sync();
  if (landed && here.path !== from) land();
}

/** `navigate` to `href` unless the reader has left `from` meanwhile, as after a slow write. */
export function navigateFrom(from: string, href: string) {
  if (here.path === from) navigate(href);
}

/**
 * Open a plain click on a link into the app in place. A modified click, a link
 * out of the app, or one to an anchor on this page stays the browser's.
 */
export function follow(event: MouseEvent) {
  if (event.defaultPrevented || event.button !== 0) return;
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  const link = (event.target as Element | null)?.closest?.("a");
  if (!link || link.target || link.origin !== location.origin) return;
  if (!inApp(link.pathname)) return;
  if (link.hash && link.pathname === location.pathname && link.search === location.search) return;
  event.preventDefault();
  navigate(link.pathname + link.search + link.hash);
}
