// The catalog names of descriptors (`descriptor_names`), as the server gives them for the reader.

import type { DescriptorDisplay } from "./api-types";

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
