# `POST /api/v1/campaign-groups`

```json
{ "name": "aa-perf", "owner": "org_0195c3e2-…" }
```

Creates an empty group. `name` is trimmed, non-empty and at most 4096
characters, and two groups may share one. `owner` is an `org_…` id, the
caller's organization when absent; a caller in none gets a `400` asking it to
name one.

The owner follows the rule of
[a campaign's create](../campaigns/create_campaign.md): the caller may create
a group for the organization it belongs to, and `manage_any_orgs` for any
organization. Answers `201` with the record
([the campaign group record](../campaign-groups.md#the-campaign-group-record)).

## Statuses

| Status | When |
|--------|------|
| 201 | The group |
| 400 | The body is not JSON, names another field, or carries an empty or overlong `name` or a malformed `owner`; `owner` is omitted by a caller in no organization; or `owner` names an organization that does not exist, and the caller could otherwise have created the group there |
| 403 | The caller holds no `submit_plans`, or may not create a group for `owner` |
