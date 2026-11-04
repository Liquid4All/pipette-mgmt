// The catalog names of descriptors (`descriptor_names`), as the server gives them for the reader.

import type { DescriptorDisplay } from "./api-types";
import { textOrder } from "./format";

/** Each digest's catalog names, as a read gives them; empty where it gives none. */
export type DescriptorNames = Record<string, DescriptorDisplay>;

/** The member key that follows the name in `display`, such as `::linux-cuda-12-8`; empty for a member with none. */
export const memberKeyOf = ({ display, name }: DescriptorDisplay): string =>
  display.startsWith(name) ? display.slice(name.length) : "";

/** The tooltip of a name: the other names of the descriptor, one on each line; empty where it has none. */
export const synonymsTitle = ({ synonyms }: DescriptorDisplay): string =>
  synonyms.length ? `Also named:\n${synonyms.join("\n")}` : "";

/** The catalog names of `digest`, or null for a digest with no name that the reader can read. */
export const nameOf = (names: DescriptorNames | null | undefined, digest: string | null | undefined,
): DescriptorDisplay | null => (digest && names?.[digest]) || null;

/** The words that a search matches a named descriptor by: its display and its synonyms. */
export const nameTexts = (name: DescriptorDisplay | null): string[] => (name ? [name.display, ...name.synonyms] : []);

/** The descriptors of a list that one catalog entry names, each with its catalog names. */
export interface CatalogGroup {
  /** The entry's name as the reader sees it: `DescriptorDisplay.name`. */
  name: string;
  members: { descriptor: string; display: DescriptorDisplay }[];
}

const COLLATOR = new Intl.Collator("en", { numeric: true, sensitivity: "base" });
/** Compare numbers naturally (`q4` before `q16`) and ignore case. Use code-point order to break ties. */
const naturally = (a: string, b: string) => COLLATOR.compare(a, b) || textOrder(a, b);

/**
 * `list` in the order the console draws it:
 * - a group for each entry that names a descriptor, by entry name, with its members by member key;
 * - then the descriptors with no name, by their text.
 *
 * Entries group by the qualified name, so `global/x` and an organization's `x` are two groups.
 */
export function byCatalogName(list: readonly string[], nameOf: (descriptor: string) => DescriptorDisplay | null,
): { named: CatalogGroup[]; unnamed: string[] } {
  const groups = new Map<string, CatalogGroup>();
  const unnamed: string[] = [];
  for (const descriptor of list) {
    const display = nameOf(descriptor);
    if (!display) {
      unnamed.push(descriptor);
      continue;
    }
    const group = groups.get(display.name) ?? { name: display.name, members: [] };
    groups.set(display.name, group);
    group.members.push({ descriptor, display });
  }
  const named = [...groups.values()].sort((a, b) => naturally(a.name, b.name));
  for (const { members } of named) {
    members.sort((a, b) => naturally(memberKeyOf(a.display), memberKeyOf(b.display))
      || naturally(a.descriptor, b.descriptor));
  }
  return { named, unnamed: unnamed.sort(naturally) };
}
