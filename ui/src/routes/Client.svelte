<script lang="ts">
  // The classic console's device page (`static/templates/client.html`): the
  // record, and the forms that administer it for a reader whose device scope
  // reaches it. Each write reads the device again, as the classic form redirected.
  import {
    account as accountOf, amendClientPermissions, approveClient, client, deleteClient, deviceOwners, rejectClient,
    setClientOwner, unsuspendClient,
  } from "../lib/api";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import DeviceCatalog from "../lib/components/DeviceCatalog.svelte";
  import { latest } from "../lib/components/guard";
  import Loaded from "../lib/components/Loaded.svelte";
  import OwnerCell from "../lib/components/OwnerCell.svelte";
  import OwnerPicker from "../lib/components/OwnerPicker.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import PermissionBoxes from "../lib/components/PermissionBoxes.svelte";
  import SettingRow from "../lib/components/SettingRow.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import StatusPill from "../lib/components/StatusPill.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import {
    CLIENT_PERMISSIONS, PERMISSION_NOTES, amendment, backlogDisagrees, backlogRows, capabilityFlags, deviceBranch, deviceTrail, listingPath, namesOwner,
  } from "../lib/devices";
  import { auditAction } from "../lib/audit";
  import { DELETES, REJECT_DEVICE } from "../lib/danger";
  import { standing } from "../lib/fleet";
  import { when } from "../lib/format";
  import { ownerOf } from "../lib/owners";
  import { toggled } from "../lib/rights";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { ClientDetail, ClientPermission } from "../lib/api-types";

  let { route }: { route: RouteOf<"client"> } = $props();

  const id = $derived(route.client_id);
  const device = new Loader<ClientDetail>();
  const work = new Busy();
  device.run(() => client(session.signer, id));
  const reread = () => device.run(() => client(session.signer, id), true);

  const record = $derived(device.data);
  const branch = $derived(record && session.account
    ? deviceBranch(record, session.account, record.owning_org_id ? session.knownOrg(record.owning_org_id) : null)
    : null);
  const listing = $derived(branch ? listingPath(branch) : null);
  $effect(() => { here.under = listing; });

  // The owner as strings, so the effects below rerun when it changes, not on every reread.
  const ownerKind = $derived(record?.owner?.type ?? "");
  const ownerId = $derived(record?.owner ? (record.owner.type === "org" ? record.owner.org_id : record.owner.user_id) : "");
  const administers = $derived(!!record?.may_administer);

  // The owner's name, only for a reader of the roster it is on (`owner_labels`
  // behind `read.covers`), which `GET /api/v1/orgs` and `GET /api/v1/users` refuse any other.
  const named = $derived(!!record && !!session.account && namesOwner(record, session.account));
  let names = $state<Record<string, string>>({});
  $effect(() => {
    const [kind, owner] = [ownerKind, ownerId];
    if (!owner || !named) return;
    const asked = kind === "org"
      ? session.resolveOrg(owner).then((org) => org?.slug)
      : accountOf(session.signer, owner).then((read) => read.contact_email);
    asked.then((name) => { if (name) names = { ...names, [owner]: name }; }).catch(() => {});
  });
  // The reader and the organizations the session holds are named without a roster read.
  const shownNames = $derived.by(() => {
    const held: Record<string, string> = {};
    for (const org of session.known) if (org) held[org.org_id] = org.slug;
    if (session.account) held[session.account.user_id] = session.account.contact_email;
    return { ...held, ...names };
  });

  // The owner field is drawn when its lookup offers somebody, which the owner bounds.
  let handsOn = $state(false);
  const offering = latest();
  $effect(() => {
    void ownerId;
    if (!administers) return;
    const mine = offering();
    deviceOwners(session.signer, id, "")
      .then((found) => { if (mine()) handsOn = found.length > 0; })
      .catch(() => { if (mine()) handsOn = false; });
  });

  // The boxes as drawn from the last read; a save diffs against what was drawn.
  let ticked = $state(new Set<ClientPermission>());
  $effect(() => {
    ticked = new Set(CLIENT_PERMISSIONS.filter((name) => record?.permissions.includes(name)));
  });
  const tick = (name: ClientPermission, on: boolean) => { ticked = toggled(ticked, name, on); };

  let picker = $state<ReturnType<typeof OwnerPicker>>();

  const act = (write: () => Promise<unknown>) => work.attempt(async () => {
    await write();
    await reread();
  });

  // Either removes the registration: back to its listing, a rejection to its pending devices.
  let left = { from: "", to: null as string | null };
  const leave = (write: () => Promise<unknown>, query = "") => async () => {
    left = { from: here.path, to: listing && `${listing}${query}` };
    await write();
  };
  const reject = leave(() => rejectClient(session.signer, id), "?status=pending");
  const remove = leave(() => deleteClient(session.signer, id));
  const gone = () => { if (left.to) navigateFrom(left.from, left.to); };

  async function handOver(event: SubmitEvent) {
    event.preventDefault();
    await work.attempt(async () => {
      if (!picker) return;
      const choice = await picker.chosen();
      await setClientOwner(session.signer, id, { owner: ownerOf(choice) });
      picker.clear();
      await reread();
    });
  }

  async function savePermissions(event: SubmitEvent, held: string[]) {
    event.preventDefault();
    const body = amendment(held, ticked);
    // A device holding nothing, saved with nothing ticked, asks for nothing.
    await act(() => (body ? amendClientPermissions(session.signer, id, body) : Promise.resolve()));
  }
</script>

<PageHead trail={branch ? deviceTrail(branch) : []} title={record?.device_name ?? id}
          action={session.account ? auditAction(session.account, id) : null} />
{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

<Loaded of={device}>
  {#snippet children(view)}
    {@const flags = capabilityFlags(view)}
    {@const backlog = backlogRows(view)}
    {@const disagrees = backlogDisagrees(view)}
    <dl>
      <dt>client_id</dt><dd class="id">{view.client_id}</dd>
      <dt>status</dt><dd><StatusPill status={view.status} /></dd>
      <dt>standing</dt><dd>{standing(view)}</dd>
      <dt>owner</dt><dd>
        {#if view.owner}<OwnerCell owner={view.owner} names={shownNames} />{:else}<span class="muted">nobody</span>{/if}
      </dd>
      <dt>organization</dt><dd>{view.organization || "—"}</dd>
      <dt>client_details</dt><dd>{view.client_details || "—"}</dd>
      {#if view.contact_email}<dt>contact_email</dt><dd>{view.contact_email}</dd>{/if}
      <dt>registered_at</dt><dd>{when(view.registered_at)}</dd>
      <dt>last_seen_at</dt><dd>{when(view.last_seen_at)}</dd>
      <dt>backlog</dt><dd>{#if view.backlog}{view.backlog.job_count}
        <span class="muted">counted {when(view.backlog.counted_at)}</span>{:else}—{/if}
        {#if backlog.length}
          {#if disagrees}
            <p class="disagrees">This breakdown disagrees with the backlog count. A run counted it
              {when(view.backlog_by_campaign?.counted_at)}.</p>
          {/if}
          <table class="backlog" class:disagrees>
            <thead><tr><th>campaign</th><th>jobs</th></tr></thead>
            <tbody>
              {#each backlog as row (row.key)}
                <tr>
                  <td>{#if row.href}<a href={row.href}>{row.label}</a>{:else}<span class="muted">{row.label}</span>{/if}</td>
                  <td class="num">{row.jobs}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </dd>
      <dt>permissions</dt><dd>{view.permissions.join(", ") || "none"}</dd>
      <dt>tags</dt><dd>{view.tags.join(", ") || "none"}</dd>
      <dt>capabilities</dt><dd>
        {#if flags.length}
          <div class="muted">{flags.length} {flags.length === 1 ? "flag" : "flags"}</div>
          <ul class="flags">{#each flags as flag (flag)}<li>{flag}</li>{/each}</ul>
        {:else}none{/if}
      </dd>
      <dt>device_form_factor</dt><dd>{view.device_form_factor || "—"}</dd>
      <dt>device_os_name</dt><dd>{view.device_os_name || "—"}</dd>
      <dt>device_os_version</dt><dd>{view.device_os_version || "—"}</dd>
      <dt>device_cpu_arch</dt><dd>{view.device_cpu_arch || "—"}</dd>
      <dt>device_os_family</dt><dd>{view.device_os_family || "—"}</dd>
      <dt>device_chip_model</dt><dd>{view.device_chip_model || "—"}</dd>
      <dt>device_ram_bytes</dt><dd>{view.device_ram_bytes || "—"}</dd>
      <dt>device_gpu_model</dt><dd>{view.device_gpu_model || "—"}</dd>
      <dt>device_npu_model</dt><dd>{view.device_npu_model || "—"}</dd>
    </dl>

    {#if session.account?.may.read_fleet}<DeviceCatalog clientId={view.client_id} />{/if}

    {#if view.may_administer}
      <div class="settings">
        <SettingsSection title="General">
          {#if view.status === "pending"}
            <SettingRow title="Approve this registration"
                        text="A registration waiting for a decision. Approving it lets the device authenticate and do what its permissions allow.">
              {#snippet control(ids)}
                <button type="button" aria-describedby={ids.text} aria-disabled={work.busy}
                        onclick={() => act(() => approveClient(session.signer, id))}>Approve</button>
              {/snippet}
            </SettingRow>
          {/if}
          {#if view.suspended}
            <SettingRow title="Clear the suspension"
                        text="Suspended: it claimed work while it still held a lease, so the claim path passes it over until the suspension is cleared.">
              {#snippet control(ids)}
                <button type="button" aria-describedby={ids.text} aria-disabled={work.busy}
                        onclick={() => act(() => unsuspendClient(session.signer, id))}>Clear the suspension</button>
              {/snippet}
            </SettingRow>
          {/if}
          <SettingRow title="Owner"
                      text="An organization holds a shared device, and it stays when whoever racked it leaves. A person holds their own, and it follows them between organizations. Either way the owner says which organization the device rolls up to, and so which vault artifacts it may fetch. Detached, it belongs to nobody, fetches nothing private, and waits among the unclaimed devices for a server admin.">
            {#snippet children()}
              <!-- Detaching is its own button, so an empty field is never read as a detach. -->
              {#if handsOn}
                <form class="bar" onsubmit={handOver}>
                  <OwnerPicker bind:this={picker} lookup={(q) => deviceOwners(session.signer, id, q)} />
                  <button type="submit" aria-disabled={work.busy}>Hand it over</button>
                </form>
              {/if}
              {#if view.owner}
                <div class="bar">
                  <button type="button" aria-disabled={work.busy}
                          onclick={() => act(() => setClientOwner(session.signer, id, { owner: null }))}>Detach it</button>
                </div>
              {/if}
            {/snippet}
          </SettingRow>
          <SettingRow title="Permissions"
                      text="What the device may do once it is approved. The queue is shared, so a device holding perform_work claims any job it matches, not only its organization's.">
            {#snippet children()}
              <form onsubmit={(event) => savePermissions(event, view.permissions)}>
                <PermissionBoxes boxes={CLIENT_PERMISSIONS.map((name) => ({ name, offered: true,
                  checked: ticked.has(name), note: PERMISSION_NOTES[name] }))} toggle={tick} />
                <button type="submit" aria-disabled={work.busy}>Save permissions</button>
              </form>
            {/snippet}
          </SettingRow>
        </SettingsSection>

        <DangerZone>
          {#if view.status === "pending"}
            <DangerRow title="Reject this registration" ask={`Reject ${view.device_name ?? view.client_id}?`}
                       pending="Rejecting…" phrase={REJECT_DEVICE} run={reject} done={gone} locked={work.busy}
                       text="Deletes the registration and frees its keypair. The device authenticates nothing; registering again makes it a new pending device." />
          {/if}
          <DangerRow title="Delete this device" ask={`Delete ${view.device_name ?? view.client_id}?`} pending="Deleting…"
                     phrase={DELETES.device} run={remove} done={gone} locked={work.busy}
                     text="Removes the registration, releases its keypair and clears its queue state. The device authenticates nothing afterwards; registering again makes it a new pending device." />
        </DangerZone>
      </div>
    {/if}
  {/snippet}
</Loaded>
