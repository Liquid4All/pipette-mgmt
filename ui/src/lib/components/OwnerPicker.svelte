<script lang="ts">
  // An owner field: an autocomplete over a signed owner lookup, which answers
  // only the owners the reader may hand the record to. A pick posts its id,
  // never the text.
  // Not a menu: a server administrator's choices are every organization and every account.
  import { exactChoice } from "../owners";
  import type { OwnerMatch } from "../api-types";
  import { latest } from "./guard";

  let { lookup, label = "new owner", placeholder = "organization handle or a person's address", onpick, invalid = false,
        describedby }: {
    /** The owners whose handle, address or id holds what was typed. */
    lookup: (q: string) => Promise<OwnerMatch[]>;
    label?: string;
    placeholder?: string;
    /** The owner picked from the list, or `null` once the text no longer reads as one. */
    onpick?: (choice: OwnerMatch | null) => void;
    invalid?: boolean;
    /** The id of what explains the field: its hint, or why it is invalid. */
    describedby?: string;
  } = $props();

  const DEBOUNCE_MS = 150;
  const uid = $props.id();
  const id = `picker-${uid}`;

  let typed = $state("");
  let shown = $state<OwnerMatch[]>([]);
  let open = $state(false);
  let active = $state(-1);
  /** The owner picked, while the field still reads its label. */
  let picked: OwnerMatch | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const asking = latest();

  async function lookUp() {
    const mine = asking();
    try {
      const answered = await lookup(typed);
      // A slower answer to an earlier keystroke is not the list for this one.
      if (!mine()) return;
      shown = answered;
      open = true;
      active = answered.length ? 0 : -1;
    } catch {
      if (mine()) close();
    }
  }

  function close() {
    open = false;
    active = -1;
  }

  function pick(index: number) {
    const choice = shown[index];
    if (!choice) return;
    picked = choice;
    typed = choice.label;
    close();
    onpick?.(choice);
  }

  function input() {
    if (picked) onpick?.(null);
    picked = null;
    clearTimeout(timer);
    timer = setTimeout(lookUp, DEBOUNCE_MS);
  }

  function keydown(event: KeyboardEvent) {
    if (!open) {
      if (event.key === "ArrowDown") lookUp();
      return;
    }
    const n = shown.length;
    if (event.key === "ArrowDown" && n) {
      event.preventDefault();
      active = (active + 1) % n;
    } else if (event.key === "ArrowUp" && n) {
      event.preventDefault();
      active = (active - 1 + n) % n;
    } else if (event.key === "Enter" && active >= 0) {
      event.preventDefault();
      pick(active);
    } else if (event.key === "Escape") {
      // Closes the list alone, not the dialog the field may sit in.
      event.preventDefault();
      close();
    }
  }

  /**
   * The owner the field names: the one picked, else the one the text names
   * exactly, which the lookup puts first however many others it matches.
   * Anything else is refused, as the classic form's `owners::typed_owner` refused it.
   */
  export async function chosen(): Promise<OwnerMatch> {
    if (picked && typed === picked.label) return picked;
    const named = exactChoice(await lookup(typed.trim()), typed);
    if (!named) throw new Error(`no organization or account you may hand it to is called \`${typed.trim()}\``);
    return named;
  }

  /** Empty the field, after its owner was handed the record. */
  export function clear() {
    typed = "";
    picked = null;
    close();
    onpick?.(null);
  }
</script>

<span class="picker">
  <input type="text" bind:value={typed} required autocomplete="off" spellcheck="false"
         aria-label={label} {placeholder} role="combobox" aria-autocomplete="list"
         aria-controls="{id}-list" aria-expanded={open} aria-invalid={invalid || undefined}
         aria-describedby={describedby}
         aria-activedescendant={open && active >= 0 ? `${id}-option-${active}` : undefined}
         oninput={input} onfocus={() => { if (!open) lookUp(); }} onblur={close} onkeydown={keydown}>
  <ul id="{id}-list" class="picker-list" role="listbox" hidden={!open}>
    {#each shown as owner, i (owner.value)}
      <!-- `mousedown`, so the pick lands before the field's blur closes the list. -->
      <li id="{id}-option-{i}" role="option" tabindex="-1" aria-selected={i === active}
          onmousedown={(event) => { event.preventDefault(); pick(i); }}>
        {owner.label}<span class="muted">{owner.type === "org" ? " organization" : " person"}</span>
      </li>
    {:else}
      <li class="muted">No owner you may hand it to matches.</li>
    {/each}
  </ul>
</span>

<style>
  .picker { position: relative; display: inline-block; }
  .picker-list { position: absolute; z-index: 20; left: 0; top: 100%; min-width: 100%;
                 max-height: 18em; overflow-y: auto; margin: 2px 0 0; padding: 4px 0;
                 list-style: none; background: var(--bg); border: 1px solid var(--border-strong);
                 border-radius: var(--radius); box-shadow: 0 2px 8px rgba(0, 0, 0, .12); }
  .picker-list li { padding: 4px 10px; white-space: nowrap; cursor: pointer; }
  .picker-list li.muted { cursor: default; }
  .picker-list li[aria-selected=true] { background: var(--accent-soft); color: var(--accent); }
  .picker-list li span { font-size: 11px; }
</style>
