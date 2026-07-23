# Vapor Docs Server

Docs serving and docs publication service for Vapor.

Initial implementation uses Axum/Tokio.

## Responsibility

- serve current docs;
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
GET  /
POST /v1/current
GET  /v1/export
```

Protected routes expect:

```text
Authorization: Bearer <VAPOR_DOCS_ADMIN_TOKEN>
```

## Non-goals

- Steam/GitHub identity authority;
- diagnostics upload storage;
- homepage/legal content ownership;
- deployment orchestration.
