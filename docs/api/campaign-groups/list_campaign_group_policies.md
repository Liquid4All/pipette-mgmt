# `GET /api/v1/campaign-groups/{group_id}/policies`

The group's policies in `policies`, one per check, in the order `swap`,
`spread`, `failed`, `ttft_over_e2e`, `model_params`, `smaller_failed`,
`vault_resource` ([policies](../campaign-groups.md#policies)). A check with no stored policy
answers its defaults, with `enabled: false` and a `null` `enabled_by` and
`enabled_at`. It takes the rights of reading the group
([who may do what](../campaign-groups.md#who-may-do-what)).

## Statuses

| Status | When |
|--------|------|
| 200 | Every check's policy |
| 403 | The group is outside the caller's reach ([who may do what](../campaign-groups.md#who-may-do-what)), or the caller holds neither `read_plans` nor `manage_orgs` |
