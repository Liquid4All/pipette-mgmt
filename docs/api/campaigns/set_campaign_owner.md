# `PUT /api/v1/campaigns/{campaign_id}/owner`

```json
{ "owner": { "type": "org", "org_id": "org_0195c3e2-…" } }
```

Hands the campaign to another account or organization. Checked at **both** ends,
under the rename's gate ([`PATCH`](update_campaign.md)) at each, the split
[`PUT /api/v1/clients/{client_id}/owner`](../devices/set_client_owner.md) takes: the
caller has to administer the campaign as it stands *and* be allowed to put one
down where it is headed. The near end is checked first, and a destination outside
the caller's reach reads the same whether or not it exists, so a refused hand-off
discloses nothing about where it was aimed.

**Where a campaign may be put down is wider than which campaigns may be taken**,
and that asymmetry is the point:

| Destination | Who may hand a campaign there |
|---|---|
| Yourself | Anyone who may administer the campaign |
| An organization you **administer** | Its `manage_orgs` holders |
| An organization you **belong to** | Any member, for their own campaign |
| A **member** of an organization you administer | Its `manage_orgs` holders |
| Anywhere else | `manage_any_orgs` |

**So only a server admin moves a campaign between organizations.** An
organization's administrator administers that organization's campaigns alone
and puts one down only on it or its members, so a move out is refused at the
far end and another organization's campaign at the near one. A member gives its
own campaign only to the one organization it belongs to; an account belongs to
at most one.

The third and fourth rows are one round trip: an administrator hands the team's
campaign down to somebody on the team, and that person can hand it back. Without
the third, the hand-down would be one-way, since a personal campaign is personal
and its former administrator can no longer reach it.

Whoever hands it over, an account takes a campaign only if it holds
`submit_plans`, the grant its owner administers it under: one without could
neither run the campaign nor hand it back, so the hand-off is refused with `400`.

**Private results stay in their organization.** Whoever reads a campaign reads
its plans' results, so a campaign whose plans ran an organization's private
benchmark or one of its vault resources goes only to that organization or one
of its members. Any other destination is refused with `409`, naming the
benchmarks and resources, whoever asks. A vault resource is placed by the
`principal_id` its job names, so deleting the record changes nothing; a
person's own resource is theirs to file anywhere and is not held to this. Some
campaigns cannot be placed and are refused with `409` too. Only
`manage_any_orgs` hands over one whose plans ran a benchmark no catalog holds
any more (a private entry since deleted), or hold a job with no stored
definition (one minted before definitions were kept) or with a vault source
that names no owner. Nobody hands over one with a job definition that cannot
be read.

What stays narrow is the other end. A colleague's personal campaign is still
theirs alone: `owner` is read as it stands, never derived through anyone's
membership, so moving an account between organizations neither hands over nor
removes the campaigns it owns.

**Not nullable.** A campaign always has an owner, so there is no clear.

The pointer and the chain survive the move: a team taking over somebody's work
inherits it mid-flight rather than starting from nothing. **The plans move with
it**, without a write to any of them: every plan the campaign filed is read and
cancelled under the new owner from the moment the hand-off lands
([who may do what](../campaigns.md#who-may-do-what)). The previous owner keeps what the new owner's
reach gives it and nothing more: handing a campaign to your organization leaves
you reading its plans, as every member does, and no longer cancelling them.
Answers the same `changed` shape
[`PATCH`](update_campaign.md) does, and `changed: false` is a success: a write asking for the state
the record is already in is safe to retry.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign, and whether the hand-off changed it |
| 400 | `owner` names an account or organization that does not exist, or an account that holds no `submit_plans`, **and** the caller could otherwise have handed it there |
| 403 | The campaign is outside the caller's scope, or the destination is |
| 404 | No such campaign |
| 409 | The campaign's plans ran an organization's private benchmark or vault resource and the destination is outside that organization, or the campaign cannot be placed and the caller is not allowed to hand it over, as above |
