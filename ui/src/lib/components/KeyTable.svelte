<script lang="ts">
  // An account's registered keys, sorted and filtered in the page, which may
  // list another table; `onrevoke` adds each key's revoke, confirmed first,
  // `current` marks the key this browser signs with, and `prior` the keys
  // registered before the owner first signed in.
  import { REVOKE_KEY, SIGN_OUT, USER_KEY_TABLE } from "../accounts";
  import { when } from "../format";
  import type { UserKey } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import DataTable from "./DataTable.svelte";
  import { Busy } from "./work.svelte";

  let { keys, current = null, prior = [], onrevoke, busy = false }: {
    keys: UserKey[];
    current?: string | null;
    prior?: string[];
    /** The revoke; a failure is drawn in its dialog. */
    onrevoke?: (key_id: string) => Promise<void>;
    /** Another write of the page is under way. */
    busy?: boolean;
  } = $props();

  const work = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  const ask = (key_id: string) => busy || work.busy || void confirm?.ask({
    ...(key_id === current ? SIGN_OUT : REVOKE_KEY(key_id)),
    run: async () => {
      await work.attempt(() => onrevoke!(key_id));
      return work.error || null;
    },
  });
</script>

{#snippet key_details(key: UserKey)}
  {key.key_details}{#if key.key_id === current}{" "}<span class="muted">(this browser)</span>{/if}
  {#if prior.includes(key.key_id)}{" "}<span class="tag warning">registered before your first recorded sign-in</span>{/if}
{/snippet}
{#snippet expires_at(key: UserKey)}
  {#if !key.expires_at}<span class="muted">never</span>
  {:else if Date.parse(key.expires_at) <= Date.now()}<span data-state="expired">expired</span>
  {:else}{when(key.expires_at)}{/if}
{/snippet}
{#snippet revoke(key: UserKey)}
  <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={busy || work.busy}
          aria-label="Revoke {key.key_id}" onclick={() => ask(key.key_id)}>Revoke</button>
{/snippet}

<DataTable spec={USER_KEY_TABLE} rows={keys} id={(key) => key.key_id} noun="keys" empty="No key registered."
           cells={{ key_details, expires_at }} action={onrevoke ? revoke : undefined} />
<ConfirmDialog bind:this={confirm} />
