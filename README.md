# Vapor Docs Server

Docs serving and docs publication service for Vapor.

Initial implementation uses Axum/Tokio.

## Responsibility

- serve current docs and static docs assets;
- accept docs publication from authorized root/deploy tooling;
- expose docs state for export/import.

## Route

Expected public route behind the root reverse proxy:

```text
/docs/
```

## State

Owns the uploaded/generated current docs tree and its metadata.

## Routes

```text
GET  /healthz
GET  /status
GET  /
POST /current
POST /current.tar.gz
GET  /export
```

`GET /status` is public and reports the current docs readiness, state path,
upload limit, and temporary auth model. It must not expose tokens.

`POST /current` accepts a single HTML document for quick smoke tests.
`POST /current.tar.gz` accepts a gzip-compressed tar archive containing a
static docs tree with `index.html` at the archive root.

Archive uploads reject absolute paths, parent-directory traversal, duplicate
normalized paths, symlinks, hardlinks, and other non-file/non-directory entries.
They also enforce separate extracted-byte and file-count limits.

## Current docs state

```text
state/docs/
  current/
    metadata.toml
    index.html
    ...
```

`GET /export` returns current metadata plus the current `index.html`. Whole
payload recovery remains covered by the root state bundle until Docs needs a
service-owned streamed export/import format.

Protected routes expect:

```text
Authorization: Bearer <VAPOR_DOCS_ADMIN_TOKEN>
```

## Non-goals

- Steam/GitHub identity authority;
- diagnostics upload storage;
- homepage/legal content ownership;
- deployment orchestration.
