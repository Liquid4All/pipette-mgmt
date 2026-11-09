// A catalog entry made from a campaign's progress table: one runtime or model descriptor as the one member of a new
// entry of the campaign's organization (`POST /api/v1/orgs/{org_id}/descriptor-catalog`). A runtime or a model can also
// become a new member of an entry that the organization has already
// (`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members`).

import { ApiError } from "./api";
import type {
  DescriptorDisplay, DescriptorEntryAction, DescriptorEntryListRow, DescriptorEntryState, DescriptorKind,
  DescriptorNameHit, NamedCampaignOverview,
} from "./api-types";
import { describe } from "./components/guard";
import { canonical } from "./report";
import { parsed, unlabelled, vaultIds, vaultOwnedBy } from "./vault";

/** The longest catalog name, as `CatalogName::MAX_LEN` allows. */
export const CATALOG_NAME_MAX = 200;

/** The most digests that one `/by-digest` read takes. */
export const DIGEST_BATCH = 100;

export function stateActions(state: DescriptorEntryState):
  { action: Exclude<DescriptorEntryAction, "tombstone">; label: string }[] {
  switch (state) {
    case "active":
      return [{ action: "pin", label: "Pin" }, { action: "hide", label: "Hide" }];
    case "pinned":
      return [{ action: "unpin", label: "Unpin" }];
    case "hidden":
      return [{ action: "unhide", label: "Unhide" }];
    case "tombstoned":
      return [];
  }
}

/** The organization where a campaign page's reader may name runtimes. */
export interface CatalogOrg {
  id: string;
  /** The organization as the reader knows it: its slug, else its name in the overview, else its id. */
  label: string;
}

/**
 * The campaign's catalog organization (`catalog_org_id`) with its label, or null where the reader may name nothing.
 * `slugOf` gives the slug of an organization that the reader knows already.
 */
export function catalogOrgOf(detail: Pick<NamedCampaignOverview, "catalog_org_id" | "names">,
                             slugOf: (org_id: string) => string | null): CatalogOrg | null {
  const id = detail.catalog_org_id;
  return id ? { id, label: slugOf(id) ?? detail.names?.[id] ?? id } : null;
}

/** Each digest's ranked live names, as `/by-digest` gives them for the reader. */
export type DigestNames = ReadonlyMap<string, readonly DescriptorNameHit[]>;

/**
 * Why `text` is not a catalog name, or null for a valid name. The grammar is the grammar of the server:
 * - a letter or a digit first;
 * - only ASCII letters, digits, spaces, `.`, `_` and `-`;
 * - a space only between two other characters;
 * - at most 200 characters.
 */
export function catalogNameError(text: string): string | null {
  if (!text) return "Type a name.";
  if (!/^[A-Za-z0-9]/.test(text)) return "Start the name with a letter or a digit.";
  if (!/^[A-Za-z0-9._ -]*$/.test(text)) return "Use only letters, digits, spaces, '.', '_' and '-' in the name.";
  if (text.includes("  ")) return "Do not put two spaces together in the name.";
  if (text.endsWith(" ")) return "Do not end the name with a space.";
  if (text.length > CATALOG_NAME_MAX) return `Use at most ${CATALOG_NAME_MAX} characters in the name.`;
  return null;
}

/** Why `text` is not a quantization label, or null for a valid label. The server takes `[a-z0-9_]+`. */
export function quantizationError(text: string): string | null {
  if (!text) return "Type a quantization label, such as q4_0.";
  if (!/^[a-z0-9_]+$/.test(text)) return "Use only lowercase letters, digits and '_' in the quantization label.";
  return null;
}

/** A change of letter case only is a valid rename. */
export const renameError = (typed: string, current: string): string | null =>
  catalogNameError(typed) ?? (typed === current ? "Type a name that differs from the current name." : null);

export const relabelError = (typed: string, current: string): string | null =>
  quantizationError(typed) ?? (typed === current ? "Type a label that differs from the current label." : null);

/** Every string in `value`, in the order of the JSON text. */
function strings(value: unknown): string[] {
  if (typeof value === "string") return [value];
  if (!value || typeof value !== "object") return [];
  return Object.values(value).flatMap(strings);
}

/**
 * A quantization in a file name, a repository name or a prefix of a model descriptor. The alternatives:
 * - a GGUF type, such as `Q4_0`, `Q4_K_M`, `IQ4_XS`, `TQ1_0`, `PQ2_0` or `PTQ1_0`;
 * - a block-scaled float type, such as `MXFP4` or `MXFP4_MOE`;
 * - a float type, such as `F16`, `BF16` or `fp16`;
 * - an MLX bit width, such as `4bit`;
 * - an ONNX or OpenVINO scheme, such as `int4-asym-gq128`.
 * `model_params.rs` `is_quant_tag` strips the same forms from a model name for the lookup of its parameters.
 * A letter or a digit on either side ends the match, so `Qwen3` and a commit hash do not match.
 */
const QUANTIZATION =
  /(?<![a-z0-9])((?:(?:i|t|pt?)?q|(?:mx|nv)fp)\d+(?:_[a-z0-9]+)*|b?f(?:16|32)|fp(?:8|16|32)|\d+bit|int\d+(?:[-_][a-z0-9]+)*)(?![a-z0-9])/gi;

/**
 * The quantization labels that the strings of the parsed model `descriptor` show, in the order of the JSON text, each
 * label once. A label is the match in lowercase, with `_` for each `-`. A descriptor with more than one file, such as
 * a vision model or a speculative pair, can give more than one label.
 */
export function quantizationHints(descriptor: unknown): string[] {
  const found = strings(descriptor).flatMap((text) =>
    [...text.matchAll(QUANTIZATION)].map((match) => match[0].toLowerCase().replaceAll("-", "_")));
  return [...new Set(found)];
}

/** The entry that `name` selects. Names compare in ASCII lowercase, as the server's `CatalogName::key` does. */
function entryNamed(name: string, entries: readonly DescriptorEntryListRow[]): DescriptorEntryListRow | undefined {
  const key = name.toLowerCase();
  return entries.find((row) => row.name.toLowerCase() === key);
}

/** The name of the entry that `name` selects, as the organization stores it. Else `name` as typed. */
export const storedName = (name: string, entries: readonly DescriptorEntryListRow[]): string =>
  entryNamed(name, entries)?.name ?? name;

/** One label in the member tree of an entry, with the labels under it. */
export interface MemberNode {
  label: string;
  children: MemberNode[];
}

/**
 * The members of `entry` as a tree, in the order of member precedence. A model entry has a node for each format, with
 * the quantizations of that format under it. A runtime entry has a leaf for each flavor, and `default` for no flavor.
 */
export function memberTree(entry: DescriptorEntryListRow): MemberNode[] {
  const leaf = (label: string): MemberNode => ({ label, children: [] });
  const formats = new Map<string, MemberNode>();
  return entry.member_summaries.flatMap(({ member_key: key }) => {
    if (key.kind === "runtime") return [leaf(key.flavor ?? "default")];
    const format = formats.get(key.format);
    if (format) {
      format.children.push(leaf(key.quantization));
      return [];
    }
    const node = { label: key.format, children: [leaf(key.quantization)] };
    formats.set(key.format, node);
    return [node];
  });
}

/**
 * What the dialog does with a typed name, given the live entries of the organization:
 * - `create`: no entry has the name, so the dialog creates one;
 * - `add`: a hand-made entry of the same descriptor kind has the name. The dialog adds the descriptor as a new member.
 *   For a runtime, the entry also holds the same build: a member that differs from the runtime only in flavor;
 * - `taken`: another entry has the name, so the dialog can do neither. `why` is `kind` if the descriptor kind differs
 *   or the entry is not hand-made. It is `build` for another runtime build.
 * Names compare in ASCII lowercase, as the server's `CatalogName::key` does.
 */
export type EntryTarget =
  | { to: "create" }
  | { to: "add"; entry: DescriptorEntryListRow }
  | { to: "taken"; why: "kind" | "build"; entry: DescriptorEntryListRow };

/**
 * A runtime descriptor without its flavor and its vault `custom_label`s, as canonical JSON: the build that the
 * server's `DescriptorEntry::validate_runtime_build` compares.
 */
function build(runtime: unknown): string {
  const value = unlabelled(runtime);
  if (typeof value !== "object" || value === null) return canonical(value);
  const { flavor: _, ...rest } = value as Record<string, unknown>;
  return canonical(rest);
}

/** `descriptor.value` is the parsed descriptor. For a runtime, `entries` carry the descriptors of their members. */
export function entryTarget(name: string, entries: readonly DescriptorEntryListRow[],
                            descriptor: { kind: DescriptorKind; value: unknown }): EntryTarget {
  const entry = entryNamed(name, entries);
  if (!entry) return { to: "create" };
  if (entry.descriptor_kind !== descriptor.kind || entry.kind !== "hand-made") return { to: "taken", why: "kind", entry };
  if (descriptor.kind === "model") return { to: "add", entry };
  const wanted = build(descriptor.value);
  return entry.members?.some((member) => build(member.descriptor) === wanted)
    ? { to: "add", entry }
    : { to: "taken", why: "build", entry };
}

/**
 * What a cell shows for the ranked names of a digest, in the shape that the jobs read gives. The selector of the first
 * name is the display, and the selectors of the others are its synonyms. Null for a digest with no name. This is the
 * rule of the server's `NameView::displays`: keep the two the same.
 */
export function displayOf(hits: readonly DescriptorNameHit[]): DescriptorDisplay | null {
  const [first, ...rest] = hits;
  return first ? { display: first.selector, name: first.display_name, synonyms: rest.map((hit) => hit.selector) } : null;
}

/**
 * Whether one of `hits` names a descriptor of `kind` in the organization `org_id`: a name of the organization, or a
 * global name. The name of another organization does not count.
 */
export const namedIn = (hits: readonly DescriptorNameHit[], org_id: string, kind: DescriptorKind): boolean =>
  hits.some((hit) => (hit.scope === org_id || hit.scope === "global") && hit.descriptor_kind === kind);

/**
 * What a runtime cell or a model cell offers:
 * - `create`: a button that creates an entry;
 * - `foreign`: a disabled button, for a model with a source outside Hugging Face and the org vault;
 * - `unpinned`: a disabled button, for a model with a Hugging Face revision that is not a commit.
 */
export type EntryOffer = "create" | "foreign" | "unpinned";

/** A commit: 40 lowercase hex characters, as the server's `GitOid` takes. */
const COMMIT = /^[0-9a-f]{40}$/;

/** Every object of `value` that has a string `source`: the sources of a model. */
function sources(value: unknown): Record<string, unknown>[] {
  if (Array.isArray(value)) return value.flatMap(sources);
  if (!value || typeof value !== "object") return [];
  const object = value as Record<string, unknown>;
  return [...(typeof object.source === "string" ? [object] : []), ...Object.values(object).flatMap(sources)];
}

/** The offer for a model that no organization entry names yet. */
function modelOffer(descriptor: string): EntryOffer {
  const found = sources(parsed(descriptor));
  if (!found.length || found.some((source) => source.source !== "huggingface")) return "foreign";
  return found.every((source) => typeof source.revision === "string" && COMMIT.test(source.revision))
    ? "create" : "unpinned";
}

/**
 * The offer for `descriptor`, a descriptor of `kind` whose digest is `digest`, in the organization `org_id`.
 * `resources` is the read's names of vault resources. The offer is null in these cases:
 * - the names are not read yet;
 * - the descriptor has no digest;
 * - the organization names the descriptor already, or a global name names it;
 * - the descriptor names a vault resource that is gone or that the organization does not own. The two read the same,
 *   so the page does not show that another owner's resource exists.
 */
export function entryOffer(descriptor: string, kind: DescriptorKind, digest: string | undefined,
                           names: DigestNames | null, org_id: string,
                           resources: Readonly<Record<string, string>>): EntryOffer | null {
  if (!digest || !names || namedIn(names.get(digest) ?? [], org_id, kind)) return null;
  if (!vaultIds(descriptor).length) return kind === "model" ? modelOffer(descriptor) : "create";
  if (!vaultOwnedBy(descriptor, org_id, resources)) return null;
  if (kind === "runtime") return "create";
  return sources(parsed(descriptor)).every((source) => String(source.source).startsWith("vault_"))
    ? "create" : "foreign";
}

/** Why a disabled button cannot create an entry. */
export function offerReason(offer: Exclude<EntryOffer, "create">): string {
  switch (offer) {
    case "foreign":
      return "The catalog names models on Hugging Face or in the organization's vault.";
    case "unpinned":
      return "This model names no commit. The campaign ran the newest commit at the time of each run, so a name "
        + "for today's commit can name a different model.";
  }
}

/** Splits `digests` into reads of at most `DIGEST_BATCH` digests. Each digest occurs once. */
export function digestBatches(digests: Iterable<string>): string[][] {
  const unique = [...new Set(digests)];
  return Array.from({ length: Math.ceil(unique.length / DIGEST_BATCH) },
    (_, i) => unique.slice(i * DIGEST_BATCH, (i + 1) * DIGEST_BATCH));
}

const WRITES = {
  entry: { noun: "entry", outcome: "created the entry" },
  member: { noun: "member", outcome: "added the member" },
  name: { noun: "name", outcome: "renamed the entry" },
  label: { noun: "label", outcome: "changed the label" },
  state: { noun: "change", outcome: "changed the state of the entry" },
} as const;

export type CatalogWrite = keyof typeof WRITES;

/**
 * What the page says after a failed `write`. For a refusal, it gives the reason from the server. For any other failure,
 * it says how to learn the outcome.
 */
export function writeFailure(e: unknown, write: CatalogWrite): string {
  const { noun, outcome } = WRITES[write];
  if (e instanceof ApiError && e.status >= 400 && e.status < 500) return `The server refused the ${noun}: ${e.message}`;
  const maybe = `Read the page again to see whether the server ${outcome}.`;
  if (e instanceof ApiError) return `The server failed (${e.message}). ${maybe}`;
  return `No answer from the server (${describe(e)}). ${maybe}`;
}
