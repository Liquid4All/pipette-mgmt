# `POST /api/v1/orgs/{org_id}/descriptor-families/{family}/logo`

Set or remove the logo of one model family in the named organization. The
model pickers of the clients show the logo with the family. The path takes the
organization identifier (ID) and a family, such as `LFM2.5`. The family
follows the catalog name grammar. The server ignores the case of the family. The caller
needs `edit_catalog` and membership, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body gives the new logo:

```json
{"logo_uri": "https://pipette.liquid.ai/logos/lfm.svg"}
```

A data URI holds the image in the body:

```json
{"logo_uri": "data:image/svg+xml;base64,PHN2Zy8+"}
```

A logo Uniform Resource Identifier (URI) has one of two forms:

- a URI under `https://pipette.liquid.ai/logos/`. After that prefix, it has
  path segments of `[A-Za-z0-9._-]` with `/` between them. A segment cannot
  be empty or start with `.`.
- a data URI that holds the image. Start with `data:`. Add `image/png`,
  `image/svg+xml`, or `image/webp`. Add `;base64,` and the image in padded
  base64 using the standard alphabet.

A logo URI has at most 65,536 characters. The complete JSON request body has
at most 65 KiB (66,560 bytes).

To set a logo, the organization must have a model entry with the family that
is not tombstoned. The entry can use the default family or a family that an
operator set. A `null` logo URI removes the logo. A removal does not need an
entry.

An organization logo applies to the organization's own entries. An
organization entry with no logo for its family shows the global logo for that
family. A logo stays after the last entry of its family is renamed or
tombstoned.

A catalog read gives the SHA-256 of the logo URI in `family_logo_sha256`.
A client gets the logo URI with that digest from
[`GET /api/v1/descriptor-catalog/family-logos/{sha256}`](get_descriptor_family_logo.md).
A new logo has a new digest, so the clients see the change on their next
catalog read.

The server commits the change and its `catalog_set_family_logo` audit event in
one transaction. A removal that finds no logo changes nothing and records no
`catalog_set_family_logo` event. The event holds the whole logo URI, so a
data URI logo makes an audit record of up to 64 KiB. The response gives the
scope, the family as the path gives it, and the new logo URI. If the audit write fails, the server rolls back the change and
returns an internal server error.

## Statuses

| Status | When |
|--------|------|
| 200 | The server set or removed the logo |
| 400 | The body is invalid, it has no `logo_uri`, or the logo URI has neither form |
| 403 | The caller lacks catalog edit permission |
| 404 | No model entry in the organization has the family |
| 413 | The request body exceeds 65 KiB |
