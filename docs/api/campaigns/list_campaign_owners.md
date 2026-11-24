# `GET /api/v1/campaigns/{campaign_id}/owners`

Whom the caller may hand the campaign to: the choices of the app's owner field,
for it to post the one picked to [`PUT …/owner`](set_campaign_owner.md),
which does not draw this boundary itself. It takes the rename's gate
([`PATCH`](update_campaign.md)). The query and the answer are those of
[`GET /api/v1/clients/{client_id}/owners`](../devices/list_client_owners.md).

The choices follow [the hand-off rule](set_campaign_owner.md): the
organization that owns the campaign, less the current owner, which is that same
organization. So the list is empty; a move between
organizations is `manage_any_orgs`, sent straight to
[`PUT …/owner`](set_campaign_owner.md).

The campaign is read first, then the caller's authority over it.

## Statuses

| Status | When |
|--------|------|
| 200 | The owners the caller may hand the campaign to |
| 400 | `campaign_id` is not a campaign id, or `type` is not `org` |
| 403 | The campaign is outside the caller's scope, or the caller may not administer it |
