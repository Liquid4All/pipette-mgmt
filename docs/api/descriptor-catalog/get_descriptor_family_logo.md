# `GET /api/v1/descriptor-catalog/family-logos/{sha256}`

Get the logo of a model family by its digest. A catalog read gives the digest
in `family_logo_sha256`. The path takes that digest: 64 lowercase hexadecimal
characters. The caller needs an active account.

The server applies the 256-bit Secure Hash Algorithm (SHA-256) to the bytes
of the logo's Uniform Resource Identifier (URI). The JavaScript Object
Notation (JSON) response gives the digest and the logo URI:

```json
{
  "sha256": "c2e7d21098f9131392a4dc3789f3b8de9561b38e3ad627389872d24a5ca4fa02",
  "logo_uri": "data:image/png;base64,abcd"
}
```

The logo URI has one of two forms:

- a URI under `https://pipette.liquid.ai/logos/`;
- a data URI that holds the image, such as `data:image/png;base64,...`.

Use the logo URI as the source of an image. The server returns a logo only
from a scope that the caller can read. That scope is the global scope, or the
caller's organization. A holder of `edit_any_catalog` reads every organization.
A device gets a logo from its own short list with
[`GET /clients/me/descriptor-catalog/family-logos/{sha256}`](../../httpapi.md#220-get-clientsmedescriptor-catalogfamily-logossha256).

The response has the header `Cache-Control: private, no-store` so HTTP caches
do not reuse a response after the caller's access changes.

## Statuses

| Status | When |
|--------|------|
| 200 | The server returns the logo |
| 400 | The digest is not 64 lowercase hexadecimal characters |
| 404 | No logo that the caller can read has the digest |
