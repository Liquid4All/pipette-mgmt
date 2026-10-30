<script lang="ts" generics="K extends PreauthKeyRow">
  // Outstanding pre-auth keys, an account's or an organization's devices',
  // each with its revoke, confirmed first, sorted and filtered in the page,
  // which lists other tables; `usage` adds a device key's usage column.
  import { when } from "../format";
  import { REVOKE_PREAUTH, type PreauthKeyRow } from "../preauth";
  import { userLink } from "../routes";
  import { session } from "../session.svelte";
  import type { TableSpec } from "../table";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import DataTable from "./DataTable.svelte";
  import { Busy } from "./work.svelte";

  let { keys, usage, busy, onrevoke, empty }: {
    keys: K[];
    usage?: (key: K) => string;
    /** Another write of the page is under way. */
    busy: boolean;
    /** The revoke; a failure is drawn in its dialog. */
    onrevoke: (key_id: string) => Promise<void>;
    /** What the table says with no key outstanding. */
    empty: string;
  } = $props();

  const work = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  const ask = (key_id: string) => busy || work.busy || void confirm?.ask({
    ...REVOKE_PREAUTH(key_id),
    run: async () => {
      await work.attempt(() => onrevoke(key_id));
      return work.error || null;
    },
  });

  // The reader names its own mints; any other minter shows as its id.
  const minter = (user_id: string) =>
    (user_id === session.account?.user_id ? session.account.contact_email : null);
  const by = (key: K) => (key.minted_by ? minter(key.minted_by) ?? key.minted_by : "the host");
  const expiry = (key: K) => (key.expired ? "expired" : key.expires_at ? "expires" : "never");

  const spec: TableSpec<K> = $derived({
    columns: [
      { key: "key_id", label: "key_id", raw: (key) => key.key_id, as: "id" },
      ...(usage ? [{ key: "usage", label: "usage", raw: (key: K) => usage(key) }] : []),
      { key: "expires_at", label: "expires_at", raw: (key) => key.expires_at ?? null },
      { key: "note", label: "note", raw: (key) => key.note || null },
      { key: "created_at", label: "created_at", raw: (key) => key.created_at, as: "time" },
      { key: "minted_by", label: "minted_by", raw: by },
    ],
    texts: [{ key: "q", label: "key_id or note", texts: (key) => [key.key_id, key.note] }],
    choices: [{ key: "expiry", label: "expires_at", values: (key) => [expiry(key)], order: ["expires", "never", "expired"] }],
  });
</script>

{#snippet expires_at(key: K)}
  {#if key.expired}<span data-state="expired">expired</span>{" "}{/if}
  {key.expires_at ? when(key.expires_at) : "never"}
{/snippet}
{#snippet minted_by(key: K)}
  {#if key.minted_by}
    {@const named = minter(key.minted_by)}
    <a class:id={!named} href={userLink(key.minted_by)} title={key.minted_by}>{named ?? key.minted_by}</a>
  {:else}the host{/if}
{/snippet}
{#snippet revoke(key: K)}
  <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={busy || work.busy}
          aria-label="Revoke {key.key_id}" onclick={() => ask(key.key_id)}>Revoke</button>
{/snippet}

<DataTable {spec} rows={keys} id={(key) => key.key_id} noun="keys" {empty} cells={{ expires_at, minted_by }}
           action={revoke} />
<ConfirmDialog bind:this={confirm} />
