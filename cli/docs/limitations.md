---
title: Limitations
section: Reference
order: 220
summary: What Cadmus does not do yet, so you know before you build.
---

Cadmus is at version 0.2. These are the gaps known today.

## Accounts and logins

- No passkeys yet.
- API keys cannot be limited to some models (only read-only).
- An account is linked to another site by e-mail address only (there is no "connect"
  button), and its e-mail address cannot be changed through the API.
- Per-record permissions are ownership only: one owner per row (see
  [Rows owned by accounts](accounts.md#rows-owned-by-accounts)), not sharing rules.
- Login attempt limits are per server process, not shared between instances. Request rate
  limits are too, unless you plug in a shared store ([Rate limits](rate-limits.md#several-instances)).
- Superusers manage accounts and groups in the admin only; through the API, people manage
  only their own account (`/api/auth/me`).
- Logging out does not end a copied access token before it expires (an hour by default);
  changing the password ends them all.
- Refresh tokens and e-mailed links for apps need the built-in accounts, not a custom
  `UserStore`.
- Facebook logins from apps need the `code`: Facebook's mobile SDK access tokens are not
  accepted.
- Google, GitHub and Keycloak (OpenID Connect) logins have been tested against the real
  services; Microsoft and Facebook against local stand-ins that follow their documented
  behaviour, not the live services.

## Admin

- No page listing all uploaded files, and a replaced file is not deleted.
- No search-as-you-type for long lists of related records: above 200, forms take ids.
- No editing of related records inside another record's form (inlines).
- Images in rich text are linked by address; they cannot be uploaded from the editor.

## Models and migrations

- Single-column primary keys only.
- Binary fields are read-only in the admin and the API.
- On SQLite, decimals are stored as floating-point numbers, exact to about 15 digits; use
  PostgreSQL or MySQL for larger exact amounts.
- Migrations do not handle database enums, or changing a primary key.
- `migrate` takes no lock: run it once per deployment, not in every copy of the server.

## Uploads

- Completing a resumable upload copies the pieces once inside the storage (no direct-to-S3
  multipart yet), and abandoned uploads are not cleaned up automatically.
- S3, Azure and Google Cloud Storage have been tested against local stand-ins (SeaweedFS,
  Azurite, fake-gcs-server), not the real services.

## Server and documentation

- Admin pages are not compressed (on purpose; see [Deploying](deploying.md#compression)).
- No request ids or metrics; add them with your own `tower` layers.
- Your own routes are not added to the API documentation automatically.
- The API documentation viewers load from a CDN rather than being bundled.

Found something else, or need one of these? Open an issue on
[GitHub](https://github.com/arnabgupta-dev/cadmus/issues).
