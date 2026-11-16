# `GET /api/v1/campaign-groups/{group_id}`

The group record with one more field, `members`, in campaign id order, which
is creation order. Each member is a
[campaign record](../campaigns.md#the-campaign-record), and one the caller may
not read is left out.

## Statuses

| Status | When |
|--------|------|
| 200 | The group and its members |
| 400 | `group_id` is not a group id |
| 403 | The group is outside the caller's reach ([who may do what](../campaign-groups.md#who-may-do-what)), or the caller holds neither `read_plans` nor `manage_orgs` |
| 404 | No such group, or the server has no `[database]` |
