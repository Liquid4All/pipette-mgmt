# `DELETE /api/v1/campaign-groups/{group_id}`

Deletes the group. Its members leave it, free to join another group, and every
campaign is kept as it was. Its policies are deleted with it. It takes the
rename's rights
([`PATCH`](update_campaign_group.md)). Answers `204` with no body.

## Statuses

| Status | When |
|--------|------|
| 204 | The group is deleted; its campaigns are kept |
| 403 | The caller may not administer the group ([who may do what](../campaign-groups.md#who-may-do-what)) |
