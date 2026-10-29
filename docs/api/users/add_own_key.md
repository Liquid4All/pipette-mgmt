# `POST /api/v1/users/me/keys`

Register another machine's key, signed by a key the account already holds: no
operator round-trip for a second laptop. Same body as
[`POST /api/v1/users/register`](register_user_key.md) minus `user_preauth_key` and `contact_email`,
plus an optional `expires_at` (RFC 3339, in the future and at most 30 days away)
after which the key signs nothing. A service account is refused with `403`: its
administrator mints it a pre-auth key instead
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)).

## Statuses

| Status | When |
|--------|------|
| 201 | Registered |
| 403 | The caller is a service account: its administrator mints it a pre-auth key instead |
