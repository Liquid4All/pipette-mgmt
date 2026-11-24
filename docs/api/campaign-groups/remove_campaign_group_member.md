# `DELETE /api/v1/campaign-groups/{group_id}/members/{campaign_id}`

Takes the campaign out of the group and keeps the campaign as it was. It takes
the rights of adding one ([`POST …/members`](add_campaign_group_members.md)),
on the group alone. Answers `204` with no body, also when the campaign was not
a member.

## Statuses

| Status | When |
|--------|------|
| 204 | The campaign is not a member, whether or not it was before |
| 400 | `group_id` is not a group id, or `campaign_id` not a campaign id |
| 403 | The caller may not file into the group owner's campaigns ([who may do what](../campaign-groups.md#who-may-do-what)) |
