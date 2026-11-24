# `GET /api/v1/campaigns/{campaign_id}/overview`

The campaign's page in one read, for the app: the record, what the caller may
do to it, and the plans it has run. It takes the listing's gate
([`GET /api/v1/campaigns`](list_campaigns.md)).

`device_time=true` adds the two device-time totals to the answer. Each total
costs one query to pipette-duckdb.

The body is `NamedCampaignOverview`. Its `history` walks back as
[`GET …/plans`](list_campaign_plans.md) does, each plan a `PlanSummary`
as a [listing](list_campaigns.md) row carries it, its cancel
marker unread. A device-time total counts each job by its
latest scored run, so a re-scored job counts in the current plan's; the
all-plans total counts a re-run, since the device spent the time again, and
leaves out a re-scored copy. Both read pipette-duckdb's `plan_jobs`, so a plan
filed since its last refresh adds nothing until the next.

The `may_*` flags are drawn, not enforced: each write still decides. A plan the
caller may not read stays a row, its id alone, so a running campaign does not
read as idle; `current_plan_id` is present exactly when the campaign runs a
plan, read or not.

`group` is the record of the [campaign group](../campaign-groups.md) the
campaign is in, or `null` when it is in none and for a caller that may not read
the group, such as a campaign reader.

`policy_deferral` holds the counts of re-runs and marks that the group's
policies deferred ([when a policy acts](../campaign-groups.md#when-a-policy-acts)).
`until` is the earliest eligible time for the revision. The pass recalculates
it when it evaluates the campaign, so it can move earlier as fewer jobs wait for
devices. `policy_deferral` is `null` if the campaign has no deferred
revision, `group` is `null`, or the group has no enabled policy that can file
a revision.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign, what the caller may do to it, and the plans it has run |
| 403 | The campaign is outside the caller's scope |
