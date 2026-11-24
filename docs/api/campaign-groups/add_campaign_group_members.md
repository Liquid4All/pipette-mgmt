# `POST /api/v1/campaign-groups/{group_id}/members`

```json
{ "campaign_ids": ["campaign_0195c3e2-…", "campaign_0195c3e3-…"] }
```

Adds campaigns to the group. The caller files into the group owner's
campaigns ([who may do what](../campaign-groups.md#who-may-do-what)), and each
campaign must exist, be one the caller files into, and have the group's owner.
A campaign named twice joins once, and one already in this group adds nothing.
While a policy of the group other than `model_params`, `smaller_failed` and
`vault_resource` is on, queue-maintenance
re-runs and marks the members
([when a policy acts](../campaign-groups.md#when-a-policy-acts)), so the
caller also needs a re-run's grants, as a
[policy write](set_campaign_group_policy.md) that leaves such a policy on
does. `model_params`, `smaller_failed` and `vault_resource` only flag, so while
only they are on the route's gate is enough.

All or nothing: one campaign refused, or one in another group, adds none. The
route checks every campaign before any joins, and the store refuses the whole
call when one is in another group. Answers `204` with no body.

## Statuses

| Status | When |
|--------|------|
| 204 | Every campaign is a member, whether or not it was before |
| 400 | `group_id` is not a group id; the body is not JSON, names another field, or carries a malformed or empty `campaign_ids` |
| 403 | The caller may not file into the group owner's campaigns, or into one of the campaigns; or a policy of the group other than `model_params`, `smaller_failed` and `vault_resource` is on and the caller lacks a re-run's grant, which the answer names |
| 404 | No such group or campaign |
| 409 | A campaign the caller files into has another owner than the group, or a campaign is in another group; the message names it |
