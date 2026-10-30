# `GET /api/v1/users/me`

The account, its status, its organization (`org_id` and its handle, `org_slug`,
both absent outside one), its expanded permission set, its registered keys, and
`may`: how far each of its authorities reaches (`none`, `own` for its
organization and itself, or `every`), with the names it may grant, and the ones
among them a service account may hold, as its routes judge them, so a client
draws from it rather than re-deriving the rules from names. A `pending` account
reaches nothing. This is the user form of `GET /clients/me`.

`may.edit_catalog` is `own` for a holder of `edit_catalog` in an organization.
It is `every` for a holder of `edit_any_catalog`, and `none` for all other
accounts. It gives the authority to change org entries of the descriptor
catalog. It does not apply to global entries.

After the account's first sign-in through Clerk, `first_clerk_sign_in` gives
its time (`at`) and the ids of the keys the account held then (`prior_keys`),
which were registered before its owner signed in
([Users: Signing in through Clerk](../users.md#signing-in-through-clerk)). It
is absent before that sign-in and on a server without `[clerk]`.

It admits an account at any status but `disabled`, and its signature may name
only its key. It is the one route a `pending` account reaches, so it can watch
for its admission the way a pending client watches `GET /clients/me`, and the
one that takes a signature without `X-Client-Id`, which is how a holder of
nothing but the key learns its account.

## Statuses

| Status | When |
|--------|------|
| 200 | The account |
