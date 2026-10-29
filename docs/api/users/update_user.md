# `PATCH /api/v1/users/{user_id}`

Admit, disable or restore an account, retype it, or scope a service account to
campaigns (`status`, `account_type`, `campaign_scope`, any of them).

A `manage_any_orgs` holder admits a `pending` account here, as
`pipette-mgmt users approve` does on the host: a requested account belongs to no
organization, so a scoped caller cannot reach it
([user routes](../users.md#user-routes)). Offboarding is a disable, since no
route deletes an account, and disabling stops every key the account holds while
enabling restores them all
([`DELETE /api/v1/users/{user_id}/keys/{key_id}`](revoke_user_key.md)).

It makes a person's account a service only while nobody has signed in as it
(the record's `awaits_sign_in`, [`GET /api/v1/users/{user_id}`](get_user.md));
the host's `users set-type` covers the rest. Making a service a person's is
always open. These, and the rank, are
[the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe).

A `campaign_scope` makes a service account a campaign reader, or replaces a
reader's campaigns; no value removes one
([A campaign reader reads its campaigns alone](../users.md#the-rules-that-make-delegation-safe)).

It answers `ChangedUserView`, the account plus `changed`
([Write answers](../../httpapi.md#16-write-answers)). The no-op is a body whose
`status`, `account_type` and `campaign_scope`, where named, are the stored ones.

## Statuses

| Status | When |
|--------|------|
| 200 | The account, and whether it changed |
| 400 | The caller disabling its own account; or a `campaign_scope` for a person's account, or retyping a campaign reader as one; or a `campaign_scope` naming a campaign that does not exist or that the account's organization does not own, or more than 100 |
| 403 | For a scoped caller, the account holds a server-wide name it lacks; or the caller is a service account; or the `campaign_scope` names a campaign whose plans the caller does not read |
| 404 | The account is outside the caller's scope or does not exist |
| 409 | Making a person's account a service once somebody has signed in as it; or making an account that holds `manage_orgs`, `manage_clients` or an `_any_` name a service; or making an account that holds a name beside `read_plans` a campaign reader |
