# `GET /api/v1/descriptor-catalog/by-digest`

Set `digests` to between one and 100 descriptor digests. Separate the digests
with commas. Each digest is a lowercase SHA-256 hash. A model digest is the
`model_descriptor_sha256` of a model descriptor.

The `names` map contains each digest that has a live name that the caller can
read. Other digests are absent. Each hit gives:

- the full selector, which starts with the `display_name`;
- the name;
- the scope;
- the `display_name`;
- the structured member key;
- the descriptor kind;
- the entry kind;
- the state.

A runtime member without a flavor uses its bare name as the selector.

The response lists the hits in this order:

1. the entries of the caller's organization;
2. global hand-made entries;
3. global stable entries;
4. global daily entries.

Within each group, the response sorts names in case-insensitive order. The
first hit gives the preferred name for the digest. The other hits are synonyms:
other names for the same digest. The lookup matches the whole digest.

## Statuses

| Status | When |
|--------|------|
| 200 | Ranked names for known digests |
| 400 | Digests are missing, invalid, or more than 100 |
| 404 | The server has no `[database]` catalog |
