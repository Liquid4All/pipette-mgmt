// The vault listings and a resource's page as the classic console drew them
// (`vault.html`, `vault_resource.html`), over `GET /api/v1/vault/resources`.

import type { BarTab } from "./bar";
import { ownedQuery, ownerName, principalOf, type OwnedBranch } from "./owned";
import { vaultLink, type OwnedPages } from "./routes";
import type { ChoiceFilter, TableColumn, TableSpec } from "./table";
import type {
  Model, ObjectState, Runtime, VaultArtifact, VaultEntry, VaultResourceDetail, VaultStoredFile,
} from "./api-types";

/** The vault's listing under each branch. */
export const VAULT_PAGES: OwnedPages = { mine: "me-vault", org: "org-vault", all: "vault" };

/** The `GET /api/v1/vault/resources` query for `branch`, with each file's state and every owner's name. */
export const vaultQuery = (branch: OwnedBranch, cursor: string | null, limit: string): string =>
  ownedQuery(branch, [["object", "true"], ...(branch.kind === "all" ? [] : [["names", "true"] as [string, string]])],
             cursor, limit);

/** Every file of an upload by its role; the rest of an artifact's fields (`type`, `flavor`) are not files. */
export const artifactFiles = (artifact: VaultArtifact | null | undefined): { role: string; file: VaultStoredFile }[] =>
  Object.entries(artifact ?? {}).flatMap(([role, file]) =>
    (typeof file === "object" && file ? [{ role, file: file as VaultStoredFile }] : []));

/** What a plan lists a resource's entry as. */
export type VaultKind = "model" | "runtime";

/** Where a vault at `base` lists its runtimes; its models are at `base` itself. */
export const runtimesPath = (base: string): string => `${base}/runtimes`;

/** A vault's tabs: its models, which it opens on, and its runtimes. */
export const vaultTabs = (base: string, kind: VaultKind): BarTab[] => [
  { key: "model", label: "Models", href: base, current: kind === "model", start: false },
  { key: "runtime", label: "Runtimes", href: runtimesPath(base), current: kind === "runtime", start: false },
];

/** Where each tab keeps its picked columns: apart, since a pick on one would hide what only the other has. */
export const VAULT_COLUMNS: Record<VaultKind, string> = {
  model: "pipette.vault.models.columns", runtime: "pipette.vault.runtimes.columns",
};

/** The kind a vault page lists, by its route. */
export const kindOfPage = (page: string): VaultKind => (page.endsWith("-runtimes") ? "runtime" : "model");

/**
 * The tab a row is listed under: a runtime's entry under Runtimes, every other under Models, so a record from before
 * uploads, which has no entry, is still listed.
 */
export const listedUnder = (row: Pick<VaultResourceDetail, "entry">): VaultKind =>
  (entryKind(row.entry) === "runtime" ? "runtime" : "model");

/** Whether the entry is a plan's model or runtime; `""` for a record from before uploads. */
export const entryKind = (entry: VaultEntry | null | undefined): VaultKind | "" =>
  (!entry ? "" : "runtime" in entry ? "runtime" : "model");

/** The model or runtime the entry names, as a job's descriptor holds it. */
export const entryDescriptor = (entry: VaultEntry | null | undefined): Model | Runtime | null =>
  (!entry ? null : "runtime" in entry ? entry.runtime : entry.model);

/** A vault path, as a source holds one under whichever key. */
const VAULT_PATH = /^\/vault\/resources\/([^/]+)\/content$/;

/** One vault source of a descriptor: the resources its paths name, and the `custom_label` it gives them. */
interface VaultSource {
  ids: string[];
  label: string | null;
}

/** Every object of `value` that holds a vault path, in the order its JSON does. */
function vaultSources(value: unknown): VaultSource[] {
  if (Array.isArray(value)) return value.flatMap(vaultSources);
  if (!value || typeof value !== "object") return [];
  const { custom_label: label, ...rest } = value as Record<string, unknown>;
  const fields = Object.values(rest);
  const ids = fields.flatMap((field) => (typeof field === "string" ? VAULT_PATH.exec(field)?.[1] ?? [] : []));
  return [
    ...(ids.length ? [{ ids, label: typeof label === "string" ? label : null }] : []),
    ...fields.flatMap(vaultSources),
  ];
}

/** A descriptor as an object, from its stored text or as it is; `null` for text that is no JSON. */
export function parsed(descriptor: unknown): unknown {
  if (typeof descriptor !== "string") return descriptor;
  try {
    return JSON.parse(descriptor);
  } catch {
    return null;
  }
}

/** `value` without its vault sources' `custom_label`: what a descriptor is, apart from what it is called. */
export function unlabelled(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(unlabelled);
  if (!value || typeof value !== "object") return value;
  const { custom_label: _, ...rest } = value as Record<string, unknown>;
  const source = Object.values(rest).some((field) => typeof field === "string" && VAULT_PATH.test(field));
  return Object.fromEntries(Object.entries(source ? rest : value).map(([key, field]) => [key, unlabelled(field)]));
}

/** Every vault resource a descriptor names, each once, in the order its JSON does. */
export const vaultIds = (descriptor: unknown): string[] =>
  [...new Set(vaultSources(parsed(descriptor)).flatMap(({ ids }) => ids))];

/** What a descriptor's vault source reads as, and the resource whose page says more. */
export interface DescriptorName {
  id: string;
  name: string;
}

/**
 * What `descriptor` reads as: each vault source's `custom_label`, or for a source without one, the name `names`
 * gives each resource it names. Each name once, under its first file: an upload's share theirs.
 */
export function namedAs(descriptor: unknown, names: Record<string, string>): DescriptorName[] {
  const seen = new Set<string>();
  return vaultSources(parsed(descriptor))
    .flatMap(({ ids, label }) => (label ? [{ id: ids[0]!, name: label }] : ids.map((id) => ({ id, name: names[id] }))))
    .flatMap(({ id, name }) => {
      if (!name || seen.has(name)) return [];
      seen.add(name);
      return [{ id, name }];
    });
}

/** A descriptor under what it reads as, as a filter's choice lists it; the descriptor alone where it has no name. */
export const namedLabel = (descriptor: string, names: Record<string, string>): string =>
  [...namedAs(descriptor, names).map(({ name }) => name), descriptor].join(" ");

/** A field as the server sends it: its JSON, `null` where it is absent. */
export const jsonText = (value: unknown): string | null => (value == null ? null : JSON.stringify(value));

/** A listing's `stored` cell: the word, and whether it is drawn as a fault. */
export function storedCell(object: ObjectState | null | undefined): { word: string; fault: boolean; muted: boolean } {
  if (!object || object.state === "unknown") return { word: "unknown", fault: false, muted: true };
  if (object.state === "present") return { word: "present", fault: false, muted: false };
  return { word: object.state, fault: true, muted: false };
}

/**
 * A vault tab's columns and filters: a model has no flavor, a runtime no alias. `owners` where its rows have several
 * owners to pick among.
 */
export function vaultTable(names: Record<string, string>, kind: VaultKind, owners = false): TableSpec<VaultResourceDetail> {
  const files = (row: VaultResourceDetail) => artifactFiles(row.artifact).map(({ file }) => file.filename);
  const descriptor = (row: VaultResourceDetail) => jsonText(entryDescriptor(row.entry));
  const only = <T>(of: VaultKind, items: T[]): T[] => (kind === of ? items : []);
  return {
    columns: [
      { key: "resource_id", label: "resource_id", raw: (row) => row.resource_id,
        href: (row) => vaultLink(row.resource_id) },
      { key: "name", label: "name", raw: (row) => row.name },
      ...only<TableColumn<VaultResourceDetail>>("model",
        [{ key: "alias", label: "alias", raw: (row) => jsonText(row.alias), as: "json" }]),
      { key: "owner", label: "owner", raw: (row) => ownerName(row.owner, names) },
      ...only<TableColumn<VaultResourceDetail>>("model", [
        { key: "total_millions", label: "total_millions", raw: (row) => row.model_params?.row?.total_millions ?? null, num: true },
        { key: "active_millions", label: "active_millions", raw: (row) => row.model_params?.row?.active_millions ?? null, num: true },
      ]),
      { key: "type", label: "type", raw: (row) => row.artifact?.type ?? null, as: "id" },
      ...only<TableColumn<VaultResourceDetail>>("runtime",
        [{ key: "flavor", label: "flavor", raw: (row) => row.artifact?.flavor ?? null, as: "id" }]),
      { key: "artifact", label: "artifact", raw: (row) => jsonText(row.artifact), as: "json" },
      { key: "sha256", label: "sha256", raw: (row) => row.sha256, as: "id" },
      { key: "bytes", label: "bytes", raw: (row) => row.bytes, num: true },
      { key: "object", label: "object.state", raw: (row) => row.object?.state ?? null },
      { key: "created_at", label: "created_at", raw: (row) => row.created_at, as: "time" },
      { key: "origin", label: "origin", raw: (row) => jsonText(row.origin), as: "json" },
      { key: "descriptor", label: "descriptor", raw: descriptor, as: "json" },
    ],
    texts: [
      { key: "id", label: "resource_id", texts: (row) => [row.resource_id] },
      { key: "q", label: "name, file, descriptor, alias or origin",
        texts: (row) => [row.name, ...files(row), descriptor(row), jsonText(row.alias), jsonText(row.origin)] },
    ],
    choices: [
      { key: "type", label: "type", values: (row) => [row.artifact?.type ?? ""] },
      ...only<ChoiceFilter<VaultResourceDetail>>("runtime",
        [{ key: "flavor", label: "flavor", values: (row) => [row.artifact?.flavor ?? ""] }]),
      { key: "object", label: "object.state", values: (row) => [row.object?.state ?? ""] },
      ...(owners ? [{ key: "owner", label: "owner", values: (row: VaultResourceDetail) => [principalOf(row.owner)],
                      name: (id: string) => names[id] ?? id }] : []),
    ],
  };
}
