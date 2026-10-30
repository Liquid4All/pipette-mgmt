# `GET /api/v1/openapi.json`

This document. It describes routes and no data, so it is open, and answers
with `Access-Control-Allow-Origin: *` so a viewer on another origin can load
it ([The OpenAPI document](../../httpapi.md#15-the-openapi-document)).

## Statuses

| Status | When |
|--------|------|
| 200 | The document, with an `ETag` |
| 304 | `If-None-Match` names the `ETag`: the server runs the same build |
