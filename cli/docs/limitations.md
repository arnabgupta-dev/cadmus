---
title: Limitations
section: Reference
order: 220
summary: What Cadmus does not do yet, so you know before you build.
---

Cadmus is at version 0.1. These are the gaps known today.

## Accounts and logins

- No passkeys yet.
- API keys cannot be limited to some models (only read-only).
- Logins through other sites are for the admin; programs use API keys or tokens. An account
  is linked to another site by e-mail address only (there is no "connect" button).
- No per-record permissions: permissions are per model.
- Login attempt limits are per server process, not shared between instances.
- Accounts and groups are managed in the admin only; there is no API for them.

## Admin

- No page listing all uploaded files, and a replaced file is not deleted.
- No many-to-many widget, no list of the records that point to a record, and no
  search-as-you-type for long lists of related records.

## Models and migrations

- Single-column primary keys only.
- UUID, decimal, enum and array fields are read-only in the admin and the API.
- Migrations do not handle database enums and array columns, or changing a primary key.

## Uploads

- Completing a resumable upload copies the pieces once inside the storage (no direct-to-S3
  multipart yet), and abandoned uploads are not cleaned up automatically.
- S3, Azure and Google Cloud Storage are built but have not been tested against the real
  services.

## Server and documentation

- No response compression (on purpose; see [Deploying](deploying.md)), request ids or
  metrics; add them with your own `tower` layers.
- Your own routes are not added to the API documentation automatically.
- The API documentation viewers load from a CDN rather than being bundled.

Found something else, or need one of these? Open an issue on
[GitHub](https://github.com/arnabgupta-dev/cadmus/issues).
