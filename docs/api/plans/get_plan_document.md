# `GET /api/v1/plans/{plan_id}/document`

Read back the plan document that a plan was filed from, as
[the request body of a filing](../campaigns.md#request-body) defines the stored
document: the filed text after the catalog-name and gated-model rewrites, a
variant edit's derived document, or a re-run's unchanged one. It holds no token.

It admits a plan exactly as [`GET /api/v1/plans/{plan_id}`](get_plan.md) does,
and it answers the same `404` for a plan outside the caller's scope.

A caller at the withheld tier gets `403` for a plan in its scope that has a
document, the account that filed it included
([fleet disclosure](../devices.md#fleet-disclosure)).

`plan_id` is the plan id (`plan-{uuid}`), in the path.

The `200 OK` body is the document, with
`Content-Type: application/toml; charset=utf-8`. It is not a JSON envelope. Each
error answers in the JSON `{"error"}` envelope. A `500` answers when the stored
document does not match the digest that the manifest records, or the store
failed.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan document, as `application/toml; charset=utf-8` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans`; or it holds neither `read_clients` nor `read_any_clients`, the caller's scope admits the plan, and the plan has a document |
| 404 | No such plan, or a plan the caller's scope does not admit: the message is `plan not found`. A plan filed as JSON job bodies has no document: the message is `plan has no source document`. The manifest names a document the store no longer holds (an older build's retention deleted it, or its ingest outran `plan_creating_max_age_secs`): the message is `plan source was collected` |
