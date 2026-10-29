# `GET /api/v1/campaigns/{campaign_id}/owners`

Whom the caller may hand the campaign to: the choices of the app's owner field,
for it to post the one picked to [`PUT …/owner`](set_campaign_owner.md),
which does not draw this boundary itself. It takes the rename's gate
([`PATCH`](update_campaign.md)). The query and the answer are those of
[`GET /api/v1/clients/{client_id}/owners`](../devices/list_client_owners.md).

The choices follow [the hand-off rule](set_campaign_owner.md): inside the
organization the campaign is in, its own or its owner's, the caller itself, its
own organization, and whom the caller's roster read reads, leaving out the
current owner and every account without `submit_plans`.

The campaign is read first, then the caller's authority over it.

## Statuses

| Status | When |
|--------|------|
| 200 | The owners the caller may hand the campaign to |
| 400 | `campaign_id` is not a campaign id, or `type` is neither `org` nor `user` |
| 403 | The campaign is outside the caller's scope, or the caller may not administer it |
| 404 | No such campaign |
