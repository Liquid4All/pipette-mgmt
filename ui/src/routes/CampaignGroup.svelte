<script lang="ts">
  // A campaign group (`GET /api/v1/campaign-groups/{group_id}`), its policies and its owner's campaigns, as tabs: its
  // Campaigns, drawn by `CampaignGroupBody` with the member writes this page runs, its Policies, one `PolicyRow` a
  // check, its Activity, their `PolicyPreview` and `RerunResults`, and its Settings, the group's rename and delete. A
  // member change or a rename reads the group again, a policy write the policies, and a model params write the preview.
  import {
    ApiError, addCampaignGroupMembers, campaignGroup, campaignGroupPolicies, campaignGroupPolicyPreview,
    campaignGroupRerunResults, campaigns, createModelParams, deleteCampaignGroup, removeCampaignGroupMember,
    setCampaignGroupPolicy, updateCampaignGroup,
  } from "../lib/api";
  import { auditAction } from "../lib/audit";
  import { checkLines, groupTabs, groupsCrumb, mayAdminister, mayFile, previewLacks } from "../lib/campaign-groups";
  import { CAMPAIGN_PAGES, labelScope } from "../lib/campaigns";
  import CampaignGroupBody from "../lib/components/CampaignGroupBody.svelte";
  import CampaignRename from "../lib/components/CampaignRename.svelte";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import PolicyPreview from "../lib/components/PolicyPreview.svelte";
  import PolicyRow from "../lib/components/PolicyRow.svelte";
  import RerunResults from "../lib/components/RerunResults.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { DELETES } from "../lib/danger";
  import { ownedQuery, ownerBranch, underTrail } from "../lib/owned";
  import { here, navigateFrom } from "../lib/route.svelte";
  import type { RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { ROUTE_PAGE_MAX, everyPage } from "../lib/table";
  import type {
    CampaignGroupPolicies, CampaignGroupPolicyPreview, CampaignGroupRerunResults, CampaignGroupView, CampaignView,
    CreateModelParamsRequest, PolicyKind, SetCampaignGroupPolicyRequest,
  } from "../lib/api-types";

  let { route }: {
    route: RouteOf<"campaign-group" | "campaign-group-policies" | "campaign-group-activity" | "campaign-group-settings">;
  } = $props();

  const id = $derived(route.group_id);
  const reader = $derived(session.account!);
  const tab = $derived(({
    "campaign-group": "campaigns", "campaign-group-policies": "policies", "campaign-group-activity": "activity",
    "campaign-group-settings": "settings",
  } as const)[route.page]);

  /** The group, its policies, and its owner's campaigns in every state: the members' plans, and what the add offers. */
  interface Drawn {
    group: CampaignGroupView;
    policies: CampaignGroupPolicies;
    listed: CampaignView[];
    names: Record<string, string>;
  }

  const page = new Loader<Drawn>();
  // shortcut: no tab draws until the owner's whole listing is read with plans; when that is slow, draw the group first.
  const read = async (): Promise<Drawn> => {
    const [group, policies] = await Promise.all([
      campaignGroup(session.signer, id), campaignGroupPolicies(session.signer, id),
    ]);
    // Another account's campaigns are read with everything in reach, which `memberOffers` narrows to the owner.
    const scope = labelScope(reader, group.owner) ?? { kind: "all" as const };
    // No `models`, which costs the server a plan document read per campaign; no `state`, so every state.
    const listed = await everyPage((cursor) =>
      campaigns(session.signer, ownedQuery(scope, [["plan", "true"]], cursor, ROUTE_PAGE_MAX)));
    return { group, policies, listed: listed.items, names: listed.names ?? {} };
  };
  page.run(read);
  // A group write changes the group alone, and a policy write the policies, so the other reads stay.
  const reread = () => page.run(async () => ({ ...page.data!, group: await campaignGroup(session.signer, id) }), true);
  const rereadPolicies = () =>
    page.run(async () => ({ ...page.data!, policies: await campaignGroupPolicies(session.signer, id) }), true);
  const setPolicy = (kind: PolicyKind) => async (body: SetCampaignGroupPolicyRequest) => {
    try {
      const { changed } = await setCampaignGroupPolicy(session.signer, id, kind, body);
      await rereadPolicies();
      return changed;
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 409)) throw e;
      // Another write landed since the read: the row then shows the policy as it is now.
      await rereadPolicies();
      throw new Error("This policy changed since it was read, so nothing was written. It now shows as stored.");
    }
  };
  // The preview reads every member's rows, so it is read on the Campaigns, Policies and Activity tabs alone, once one
  // is drawn and not again on a step between them, and again after a policy write, which changes what it counts. A
  // reader the route would refuse is not asked.
  const preview = new Loader<CampaignGroupPolicyPreview>();
  const lacks = $derived(previewLacks(reader));
  const readPreview = (keep = false) => preview.run(() => campaignGroupPolicyPreview(session.signer, id), keep);
  const previewed = $derived(tab !== "settings");
  $effect(() => {
    if (page.data && previewed && !lacks.length) readPreview();
  });
  // The re-run results show on the Activity tab alone, so each step to it reads them, after a policy write too; they
  // take the preview's grants.
  const results = new Loader<CampaignGroupRerunResults>();
  $effect(() => {
    if (page.data && tab === "activity" && !lacks.length) {
      results.run(() => campaignGroupRerunResults(session.signer, id));
    }
  });
  // A new entry fills the rows the preview counts at once.
  const addParams = async (body: CreateModelParamsRequest) => {
    try {
      await createModelParams(session.signer, body);
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 409)) throw e;
      // Another write added the entry since the read: the preview then shows its rows filled.
      await readPreview(true);
      throw new Error(`${body.model_key} has model params already, so nothing was written.`);
    }
    await readPreview(true);
  };

  const under = $derived(page.data ? ownerBranch(reader, page.data.group.owner, session.known, CAMPAIGN_PAGES) : null);
  const crumb = $derived(under && groupsCrumb(under));
  // Where the delete lands: the Groups tab that lists the group, else the listing it sits under.
  const landing = $derived(crumb?.href ?? under?.href ?? null);
  // Set again on each tab, as every step to another address clears it.
  $effect(() => {
    void here.path;
    here.under = under?.href ?? null;
  });
  // The reader and the organizations the session holds are named without a roster read.
  const names = $derived({ ...page.data?.names, ...Object.fromEntries([
    ...session.known.flatMap((org) => (org ? [[org.org_id, org.slug]] : [])),
    [reader.user_id, reader.contact_email],
  ]) });

  let from = "";
  const drop = async () => {
    from = here.path;
    await deleteCampaignGroup(session.signer, id);
  };
</script>

<PageHead trail={under ? [...underTrail(under, "Campaigns"), ...(crumb ? [crumb] : [])] : []}
          title={page.data?.group.name ?? id} action={auditAction(reader, id)} />

<Loaded of={page}>
  {#snippet children({ group, policies, listed })}
    {@const admin = mayAdminister(reader, group.owner)}
    <TabBar label="Campaign group" items={groupTabs(group.group_id, tab, admin)} />
    {#if tab === "campaigns"}
      <CampaignGroupBody {group} {listed} {preview} {lacks} {reader} {names} listing={under?.href}
                         add={(campaign_ids) => addCampaignGroupMembers(session.signer, id, { campaign_ids }).then(reread)}
                         remove={(campaign_id) => removeCampaignGroupMember(session.signer, id, campaign_id).then(reread)} />
    {:else if tab === "policies"}
      {@const files = mayFile(reader, group.owner)}
      <p class="notice">
        Every queue-maintenance run applies the policies that are on to each campaign's current plan, jobs that
        finished before a policy was turned on included: one revision per campaign re-runs or marks failed the jobs
        they flag, as each policy says. A campaign gets no new one while devices have not taken the jobs of the last.
        Turn on first shows what the policy alone would do now, so it is offered once the preview, which the Activity
        tab shows, has loaded.
      </p>
      {#if !files}<p class="muted">Changing these policies takes the right to file plans for this group's owner.</p>
      {:else if lacks.length}
        <p class="muted">Turn on shows the preview first, which takes
          {#each lacks as grant, index (grant)}{#if index}{" and "}{/if}<code>{grant}</code>{/each}.</p>
      {:else if preview.error}<p class="err" role="alert">Turn on shows the preview first, which failed: {preview.error}</p>
      {:else if !preview.data}<p class="muted">Reading the preview Turn on shows first…</p>
      {/if}
      <div class="settings">
        <SettingsSection title="Policies">
          {#each policies.policies as policy (policy.kind)}
            <PolicyRow {policy} save={files ? setPolicy(policy.kind) : undefined}
                       counts={preview.data ? checkLines(policy.kind, preview.data.members, group.members) : undefined} />
          {/each}
        </SettingsSection>
      </div>
    {:else if tab === "activity"}
      <PolicyPreview of={preview} {lacks} members={group.members} policies={policies.policies} {reader} add={addParams} />
      <RerunResults of={results} {lacks} groupId={group.group_id} members={group.members} policies={policies.policies} />
    {:else if admin}
      <div class="settings">
        <SettingsSection title="General">
          <!-- The route checks a group's name as a campaign's (`check_name`). -->
          <CampaignRename title="Group name" name={group.name}
                          save={(to) => updateCampaignGroup(session.signer, id, { name: to }).then(reread)} />
        </SettingsSection>
        <DangerZone>
          <DangerRow title="Delete this group" ask={`Delete ${group.name}?`} pending="Deleting…" phrase={DELETES.group}
                     text="Deletes the group and its policies. Its campaigns are kept as they are, and each is free to join another group."
                     run={drop} done={() => { if (landing) navigateFrom(from, landing); }} />
        </DangerZone>
      </div>
    {:else}
      <p class="muted">You may not rename or delete this group: that takes the right to rename its owner's campaigns.</p>
    {/if}
  {/snippet}
</Loaded>
