<script lang="ts">
  // The classic console's devices listing (`clients.html`) under any branch:
  // the fleet's clause filters, which the server applies, every page of
  // `GET /api/v1/clients` narrowed to the branch, filtered further, sorted and paged
  // in `DataTable`, and the two columns only some branches draw: who an
  // organization holds a device through, and the server's claim.
  import type { Snippet } from "svelte";
  import { account, clients } from "../api";
  import { CLAUSES, deviceTable, emptyNotice, fleetQuery, readFilters, throughOwners, type DeviceBranch } from "../fleet";
  import { offsetPaging } from "../pager";
  import { when } from "../format";
  import { bytesText } from "../plans";
  import { here, href, navigate } from "../route.svelte";
  import { session } from "../session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, tableState, withCursor, type CursorPage } from "../table";
  import type { ClientListRow, ClientStatus } from "../api-types";
  import ClauseFilters from "./ClauseFilters.svelte";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import OwnerCell from "./OwnerCell.svelte";
  import { Loader } from "./work.svelte";

  let { branch, path, claim, error = "" }: {
    branch: DeviceBranch;
    /** The page's own address, which the filters and the pager keep. */
    path: string;
    claim?: Snippet<[ClientListRow]>;
    /** A write's failure, drawn between the filters and the rows. */
    error?: string;
  } = $props();

  const STATUSES: ClientStatus[] = ["pending", "approved"];

  const filters = $derived(readFilters(here.query));
  // The page size is the table's; the server reads its largest pages.
  const asked = $derived(fleetQuery(branch, { ...filters, limit: ROUTE_PAGE_MAX }, null));

  // Read again whenever the query it sends changes, not on a filter the table applies.
  const read = new Loader<CursorPage<ClientListRow>>();
  const every = (query: string) => everyPage((cursor) => clients(session.signer, withCursor(query, cursor)));
  $effect(() => {
    const query = asked;
    read.run(() => every(query));
  });

  /** Read the rows again in place, after a write changed one. */
  export function reload() {
    const query = asked;
    return read.run(() => every(query), true);
  }

  const owned = $derived(branch.kind === "org");
  const count = $derived(branch.kind === "org" && branch.count !== null ? `Registered clients: ${branch.count}` : "");

  // The `owned` column's addresses: only the members the rows name, as
  // `owner_labels` does, each under the roster read this page already takes.
  const names = $state<Record<string, string>>({});
  const requested = new Set<string>();
  $effect(() => {
    const rows = read.data?.items;
    if (!owned || !rows) return;
    for (const user_id of throughOwners(rows).filter((id) => !requested.has(id))) {
      requested.add(user_id);
      // Unnamed, the id stands in.
      account(session.signer, user_id).then((view) => (names[user_id] = view.contact_email)).catch(() => {});
    }
  });

  // When the rows were last read: the `activity` column measures each last-seen time against it.
  let seen = $state(Date.now());
  $effect.pre(() => {
    void read.data;
    seen = Date.now();
  });
  const spec = $derived(deviceTable(owned, names, seen));
  /** The clause filters' pairs as the address holds them, which the table's links keep. */
  const serverPairs = $derived([...here.query].filter(([key]) =>
    key === "status" || CLAUSES.some((clause) => clause.key === key)));

  // `status=` is every status, where no status at all is the default. The table's sort is kept.
  const apply = (pairs: [string, string][]) =>
    navigate(href(path, [...pairs.filter(([key]) => key !== "limit"), ["sort", tableState(spec, here.query).sort],
                         ["limit", offsetPaging(here.query).asked]], ["status"]));
</script>

{#snippet ownedCell(row: ClientListRow)}
  {#if row.owner?.type === "user"}through <OwnerCell owner={row.owner} {names} />{:else}directly{/if}
{/snippet}
{#snippet backlog(row: ClientListRow)}
  {#if row.backlog}<span title="counted {when(row.backlog.counted_at)}">{row.backlog.job_count}</span>{:else}—{/if}
{/snippet}
{#snippet ram(row: ClientListRow)}
  {#if row.device_ram_bytes}<span title="{row.device_ram_bytes} bytes">{bytesText(row.device_ram_bytes)}</span>{:else}—{/if}
{/snippet}

<ClauseFilters {filters} statuses={STATUSES} onapply={apply} note={count} />

{#if error}<p class="err" role="alert">{error}</p>{/if}

<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <p class="notice">{listed.unreadable} device record{listed.unreadable === 1 ? "" : "s"} could not be read;
        the server log names them.</p>
    {/if}
    {#if listed.items.length}
      <DataTable {spec} rows={listed.items} id={(row) => row.client_id} noun="devices" {path} kept={serverPairs}
                 keep={["status"]} empty="No devices at this position." columnsKey="pipette.devices.columns"
                 cells={{ owned: ownedCell, backlog, device_ram_bytes: ram }} action={claim} actionHead="claim" />
    {:else}
      <p class="notice">{emptyNotice(branch, filters, null)}</p>
    {/if}
  {/snippet}
</Loaded>
