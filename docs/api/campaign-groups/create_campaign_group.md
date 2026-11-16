# `POST /api/v1/campaign-groups`

```json
{ "name": "aa-perf", "owner": "org_0195c3e2-…" }
```

Creates an empty group. `name` is trimmed, non-empty and at most 4096
characters, and two groups may share one. `owner` is a `user_…` or `org_…` id,
the caller when absent.

The owner follows the rule of
[a campaign's create](../campaigns/create_campaign.md): the caller may create
a group for itself, for the organization it belongs to, or for a member of the
organization it administers, and `manage_any_orgs` for anybody. An account
that owns a group holds `submit_plans`, as an account that owns a campaign
does. Answers `201` with the record
([the campaign group record](../campaign-groups.md#the-campaign-group-record)).

## Statuses

| Status | When |
|--------|------|
| 201 | The group |
| 400 | The body is not JSON, names another field, or carries an empty or overlong `name` or a malformed `owner`; or `owner` names an account or organization that does not exist, or an account that holds no `submit_plans`, and the caller could otherwise have created the group there |
| 403 | The caller holds no `submit_plans`, or may not create a group for `owner` |
| 404 | The server has no `[database]` |
