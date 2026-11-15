---
title: Upgrading
section: Reference
order: 230
summary: What changes when a project moves to a new version of Cadmus, and what to check.
---

## 0.2.1

Nothing to change in a project. `POST /api/auth/<name>` and `/api/auth/link` now answer
`404` when the login method's feature is not built in (0.2.0 answered `405`), the same as
for a method that is not listed. The documentation is corrected: a GitHub OAuth app takes
several callback URLs, so one app serves the admin and your own app.

## From 0.1 to 0.2

0.2 lets the users of your own apps log in through the REST API with every login method
(see [Logins for apps](app-logins.md)), limits request rates, and builds in only the parts of
Cadmus a project uses.

To upgrade:

1. In `Cargo.toml`, change the version to `0.2` and name the features the project uses.
   Only `sqlite` is on by default now; database drivers, login methods other than passwords,
   SMTP and cloud storage are features:

   ```toml
   cadmus = { version = "0.2", default-features = false, features = ["postgres", "google", "magic-link", "smtp", "s3"] }
   ```

   | You use | Feature |
   |---|---|
   | a PostgreSQL, MySQL/MariaDB or SQLite `DATABASE_URL` | `postgres`, `mysql`, `sqlite` |
   | `AuthMethod::Google`, `Microsoft`, `GitHub`, `Facebook`, `Oidc` | `google`, `microsoft`, `github`, `facebook`, `oidc` |
   | `AuthMethod::MagicLink` | `magic-link` |
   | `mail.url` (or `SmtpMailer` in your code) | `smtp` |
   | an `s3://`, `az://` or `gs://` storage URL | `s3`, `azure`, `gcs` (as before) |

   A feature left out stops the server at startup with the line to add, so `cargo run --
   check` tells you what is missing. `cadmus add <feature>` edits the line for you.
2. Run `cargo run -- migrate`: it adds the `auth_refresh_token` table
   (`auth.0003_refresh_tokens`).

Check these:

- **Accounts that are not staff can now log in through the API.** Staff now only decides
  who may use the admin. If you unticked **Staff** to lock someone out, untick **Active**
  instead before upgrading.
- **Password logins return a refresh token** next to `access_token`. Clients that ignore it
  keep working.
- **Two-factor accounts that log in without a code** get `401` with code `totp_required` and
  a `challenge` (it was a plain `unauthorized`). With `totp_required` on, an account without
  a key gets `403` `totp_setup_required` once its password is right.
- **`OAuthClient` has a new field, `audiences`.** Code that builds it with
  `OAuthClient { client_id, client_secret }` must use `OAuthClient::new(...)` (or add
  `audiences: Vec::new()`).
- **OIDC provider names** cannot be `token`, `totp`, `refresh`, `logout`, `me`, `link` or
  `register`, which the API now uses under `/api/auth/`.
- **Requests are now rate limited**: 120 a minute per address for visitors, 600 a minute per
  account or API key, answered with `429` beyond that. Programs that call the API harder,
  or a site behind a proxy without `server.client_ip_header`, need higher limits or
  `rate_limit: RateLimitSettings { enabled: false, ..Default::default() }` (see
  [Rate limits](rate-limits.md)). Code that builds `Settings { ... }` without
  `..Default::default()` must add `rate_limit`.
- **Deactivating an account deletes its API keys and refresh tokens**, so reactivating it
  does not bring them back; make new keys after reactivating.
- **A file that a record still names cannot be deleted** (`DELETE /api/files/{id}` answers
  `409`): clear the field first.
- **A resumable upload can only be continued, inspected or cancelled by the account that
  started it.**
- **An S3 endpoint given as `http://`** (MinIO, SeaweedFS, LocalStack) now works without
  `AWS_ALLOW_HTTP=true`.
- **`check` and `serve` warn when `auth.signup_groups` names a group that does not exist.**
- The API documentation lists the new login endpoints. A front end on another origin needs
  it in `server.cors_origins`, as before; `X-API-Key` is now allowed across origins.
- `POST /api/auth/token` still returns `token` next to `access_token`, but it is deprecated:
  read `access_token`.
