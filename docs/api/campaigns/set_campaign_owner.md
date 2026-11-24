# `PUT /api/v1/campaigns/{campaign_id}/owner`

```json
{ "owner": { "type": "org", "org_id": "org_0195c3e2-…" } }
```

Hands the campaign to another organization. Checked at **both** ends,
under the rename's gate ([`PATCH`](update_campaign.md)) at each, the split
[`PUT /api/v1/clients/{client_id}/owner`](../devices/set_client_owner.md) takes: the
caller has to administer the campaign as it stands *and* be allowed to put one
down where it is headed. The near end is checked first, and a destination outside
the caller's reach reads the same whether or not it exists, so a refused hand-off
discloses nothing about where it was aimed.

| Destination | Who may hand a campaign there |
|---|---|
| The organization you belong to | Its members holding `submit_plans` and its `manage_orgs` holders, for a campaign it already owns: a no-op |
| Any other organization | `manage_any_orgs` |

**So only a server admin moves a campaign between organizations.** An
organization's administrator administers that organization's campaigns alone
and puts one down only on it, so a move out is refused at the far end and
another organization's campaign at the near one.

**Private results stay in their organization.** Whoever reads a campaign reads
its plans' results, so a campaign whose plans ran an organization's private
benchmark or one of its vault resources goes only to that organization. Any
other destination is refused with `409`, naming the benchmarks and resources,
whoever asks. A vault resource is placed by the `principal_id` its job names,
so deleting the record changes nothing. Some
campaigns cannot be placed and are refused with `409` too. Only
`manage_any_orgs` hands over one whose plans ran a benchmark no catalog holds
any more (a private entry since deleted), or hold a job with no stored
definition (one minted before definitions were kept) or with a vault source
that names no owner. Nobody hands over one with a job definition that cannot
be read.

**Not nullable.** A campaign always has an owner, so there is no clear.

The pointer and the chain survive the move: an organization taking over work
inherits it mid-flight rather than starting from nothing. **The plans move with
it**, without a write to any of them: every plan the campaign filed is read and
cancelled under the new owner from the moment the hand-off lands
([who may do what](../campaigns.md#who-may-do-what)). The previous owner keeps what the new owner's
reach gives it and nothing more.
The move takes the campaign out of its [campaign group](../campaign-groups.md),
whose members have the group's owner; a hand-off that changes nothing does too
when the group has another owner, so a retry finishes a removal that failed.
Answers the same `changed` shape
[`PATCH`](update_campaign.md) does, and `changed: false` is a success: a write asking for the state
the record is already in is safe to retry.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign, and whether the hand-off changed it |
| 400 | `owner` names an organization that does not exist, **and** the caller could otherwise have handed it there |
| 403 | The campaign is outside the caller's scope, or the destination is |
| 409 | The campaign's plans ran an organization's private benchmark or vault resource and the destination is outside that organization, or the campaign cannot be placed and the caller is not allowed to hand it over, as above. Or another write changed the campaign while the hand-off was checked: nothing was written; send it again |
