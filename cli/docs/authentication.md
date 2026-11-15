---
title: Logging in - tokens and API keys
section: Guides
order: 90
summary: How people and programs authenticate - the admin login, login tokens for apps, and API keys for scripts and services.
---

There are two kinds of visitors, and each gets its own way in.

## People

People log in with a username (or e-mail address) and password, or another
[login method](login-methods.md) such as Google, plus their two-factor code if they turned
it on.

In the admin this happens on the login page. The login is kept in an `HttpOnly`,
`SameSite=Strict` cookie, and every form carries a CSRF token. It lasts an hour
(`auth.token_lifetime`).

An app that people use, such as a web front end or a mobile app, logs them in through the
API, with a password or any other method (see [Logins for apps](app-logins.md)), and gets a
**login token** (a JWT) and a **refresh token**:

```sh
curl -X POST http://localhost:3000/api/auth/token \
     -H "Content-Type: application/json" \
     -d '{"username": "admin", "password": "...", "totp": "123456"}'
```

```json
{ "access_token": "eyJ...", "token_type": "Bearer", "expires_in": 3600,
  "refresh_token": "cadmus_rt_...", "refresh_expires_in": 2592000 }
```

Then it sends the token with each request, and when it expires trades the refresh token for
a new pair at `POST /api/auth/refresh`:

```sh
curl -H "Authorization: Bearer eyJ..." http://localhost:3000/api/post
```

`totp` is needed only for accounts with two-factor login; a recovery code works in its
place. Without it, such an account answers `401` with code `totp_required` and a
`challenge` to finish at `POST /api/auth/totp`. The endpoint also accepts a form
(`application/x-www-form-urlencoded`).

### When a login ends

The admin cookie and login tokens are the same thing: a token signed with `SECRET_KEY`
(HS256), which says who logged in and when. Every request checks it again against the
account, so a login ends as soon as any of these happens:

| What happens | Effect |
|---|---|
| The token is older than `auth.token_lifetime` (an hour by default) | refused; an app refreshes it, the admin logs in again. |
| The password is changed (on the account's page, by a superuser, with `changepassword`, or through a reset link) | every login made with the old password ends, refresh tokens included, in every browser and app. Changing your own password in the admin keeps that one browser logged in. |
| The account is deactivated or deleted | every login, refresh token and API key stops working at once. If it is reactivated, unexpired logins and keys work again: change the password and revoke its keys if they may have leaked. |
| The account loses staff status | it can no longer use the admin; its API logins and keys keep working. |
| Its permissions change | the next request already uses the new ones. |
| `SECRET_KEY` changes | everyone is logged out. |

**Log out** in the admin removes the cookie from that browser; `POST /api/auth/logout` with
the refresh token ends an app's login. A login token copied elsewhere keeps working until it
expires: to end every login of an account at once, change its password (or deactivate it).

## Programs

Programs running on their own have nobody to type a password or a phone code: a nightly
import script, another company's server, a website's backend fetching posts to show. They use
an **API key**: a long random string, made once and put in the program's configuration.

```sh
curl -H "Authorization: Bearer cadmus_..." https://example.com/api/post
curl -H "X-API-Key: cadmus_..." https://example.com/api/post     # the same
```

| | Login token | API key |
|---|---|---|
| For | a person, in an app | a program running on its own |
| Made by | logging in with password (and two-factor code) | once, on the account's API keys page |
| Lasts | an hour | until revoked, or 30 days / 90 days / a year / never |
| If it leaks | change the password: that ends every login of the account | revoke that one key; nothing else changes |
| Can be limited | no | read-only |

## API keys

- **Making one:** each account has an **API keys** page (the key icon under its name in the
  admin's menu), or run `cargo run -- apikey --username U --name "import script"`. The key is
  shown once; only a fingerprint (SHA-256) of it is stored.
- **What it can do:** act as its account, with the account's permissions, through the API
  only, never the admin pages. A **read-only** key can view but not add, change or delete.
- **When it stops:** when revoked, when it expires, or when the account is deactivated or
  deleted. The page shows when each key was last used.
- **Safety:** making a key needs a login in the last 15 minutes or the password, so someone
  who steals a logged-in browser session cannot quietly make one. Superusers see every
  account's keys on its user page, and can revoke them.
- **Turning them off:** `auth.api_keys: false` in the settings. Existing keys then stop
  working and the page is hidden.

## Which to use

- Only people use your site through the admin: nothing to do.
- People use an app you build on the API: login tokens.
- A script or another service calls the API: an API key, read-only if it only reads.

## Related

- [Login methods](login-methods.md): Google, Microsoft, GitHub, Facebook, any OpenID Connect
  provider, and login links by e-mail.
- [Two-factor login](two-factor.md) and [login attempt limits](two-factor.md).
