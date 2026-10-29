# `GET /api/v1/users`

The accounts in scope, in one page
([Pagination](../../httpapi.md#13-pagination)), each with its resolved and stored permission sets
([account administration](../users.md#account-administration)). A scoped caller
sees the accounts in its own organization; one outside it is absent from the
listing ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)).

## Statuses

| Status | When |
|--------|------|
| 200 | The accounts in scope |
