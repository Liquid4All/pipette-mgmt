# `GET /api/v1/plans/{plan_id}/document`

Read back the plan document that a plan was filed from. The server stores the
document byte for byte when
[`POST /api/v1/campaigns`](../campaigns/create_campaign.md) or
[`POST /api/v1/campaigns/{campaign_id}/plans`](../campaigns/file_plan.md)
receives it, and this route returns those bytes. For a
revision that
[`POST /api/v1/campaigns/{campaign_id}/variants`](../campaigns/edit_variants.md)
filed, it returns the document that edit derived. For one that
[`PUT /api/v1/campaigns/{campaign_id}/document`](../campaigns/revise_document.md)
filed, it returns the document as the caller sent it. For one that
[`POST /api/v1/campaigns/{campaign_id}/rerun`](../campaigns/rerun_jobs.md)
filed, it returns the re-run plan's document, unchanged. A document whose
models carried a token is the exception. The server stores and returns such a
document with each imported model as its vault arm. The document holds no token
([filing a plan document](../campaigns.md#filing-a-plan-document)).

A **user** route. It gates on `read_plans`, widened by `read_any_plans`
([permissions](../users.md#permissions)). It admits a plan
exactly as
[`GET /api/v1/plans/{plan_id}`](get_plan.md) does,
and it answers the same `404` for a plan outside the caller's scope.

The document names client ids and requirement sets, so the route also needs the
full fleet-detail tier: `read_clients` or `read_any_clients`
([fleet disclosure](../devices.md#fleet-disclosure)). A caller
at the withheld tier gets `403` for a plan in its scope that has a document.
This rule applies to the account that filed the plan too.

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
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans`; or it holds neither `read_clients` nor `read_any_clients`, the caller's scope admits the plan, and the plan has a document |
| 404 | No such plan, **or** a plan the caller's scope does not admit: the message is `plan not found`. A plan filed as JSON job bodies has no document: the message is `plan has no source document`. The manifest names a document the store no longer holds (an older build's retention deleted it, or its ingest outran `plan_creating_max_age_secs`): the message is `plan source was collected` |
