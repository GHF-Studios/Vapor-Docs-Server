# Vapor Docs Server

Docs serving and docs publication service for Vapor.

Initial implementation uses Axum/Tokio.

## Responsibility

- serve current docs and static docs assets;
- serve versioned docs;
- accept docs publication from authorized root/deploy tooling;
- expose docs state for export/import.

## Route

Expected public route behind the root reverse proxy:

```text
/docs/
```

## State

Owns uploaded/generated docs artifacts and docs version/current pointer
metadata.

## Initial routes

```text
GET  /healthz
GET  /v1/status
GET  /
POST /v1/current
POST /v1/current.tar.gz
GET  /v1/export
```

`GET /v1/status` is public and reports the current docs readiness, state path,
upload limit, and temporary auth model. It must not expose tokens.

`POST /v1/current` accepts a single HTML document for quick smoke tests.
`POST /v1/current.tar.gz` accepts a gzip-compressed tar archive containing a
static docs tree with `index.html` at the archive root. Archive uploads reject
absolute paths, parent-directory traversal, symlinks, hardlinks, and other
non-file/non-directory entries.

Protected routes expect:

```text
Authorization: Bearer <VAPOR_DOCS_ADMIN_TOKEN>
```

## Non-goals

- Steam/GitHub identity authority;
- diagnostics upload storage;
- homepage/legal content ownership;
- deployment orchestration.
