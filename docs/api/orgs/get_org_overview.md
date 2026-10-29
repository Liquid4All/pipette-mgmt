# `GET /api/v1/orgs/{org_id}/overview`

What the organization's page draws, in one read: the body is `OrgOverview`, the object
[`GET /api/v1/orgs/{org_id}`](get_org.md) returns plus what waits on a decision and its
newest campaigns, each the row `GET /api/v1/campaigns?plan=true` answers for
it ([`GET /api/v1/campaigns`](../campaigns/list_campaigns.md)).

The gate is `GET /api/v1/orgs/{org_id}`'s: `read_orgs`, and `404` outside the
caller's scope. Every other part reads the listing it counts, as that caller:
the devices are `null` without `read_clients`, and the campaigns ([campaigns](../campaigns.md)), the
vault ([the vault](../vault.md)) and the catalog ([private benchmarks](../orgs.md#private-benchmarks))
are each `null` where their own route answers that caller `403`; any other
failure fails the whole read. A count is the caller's: where an organization is
outside a listing's reach, that listing filters it out, so it counts `0` rather
than `null`. `may_administer` and `may_delete` are drawn, not enforced: the
writes decide again. It makes **several listings per call**, so it is a page's
read and not one to poll.

## Statuses

| Status | When |
|--------|------|
| 404 | The organization is outside the caller's scope or does not exist |
