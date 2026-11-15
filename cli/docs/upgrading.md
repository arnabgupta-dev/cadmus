---
title: Upgrading
section: Reference
order: 230
summary: What changes when a project moves to a new version of Cadmus, and what to check.
---

## From 0.1 to 0.2

0.2 lets the users of your own apps log in through the REST API with every login method
(see [Logins for apps](app-logins.md)). To upgrade, change `cadmus = "0.1"` to
`cadmus = "0.2"` in `Cargo.toml`, then run `cargo run -- migrate`: it adds the
`auth_refresh_token` table (`auth.0003_refresh_tokens`).

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
- The API documentation lists the new login endpoints. A front end on another origin needs
  it in `server.cors_origins`, as before.
