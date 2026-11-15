---
title: Logins for apps
section: Guides
order: 105
summary: Log the users of your web or mobile app in through the REST API - passwords, Google and other sites, login links, sign-up, two-factor and refresh tokens.
---

Every [login method](login-methods.md) works for apps too: a React, Next.js, Vue or mobile
front end logs its users in through the REST API and gets tokens. The admin is not involved,
and these users need not be staff.

## Who may log in

Any **active** account may log in through the API. **Staff** only decides whether it may
also use the admin. An account that is not staff gets `403` on every admin page, whatever it
logs in with, and its permissions (and [owned rows](accounts.md#rows-owned-by-accounts))
apply in the API as usual.

People who sign up through the API (with a provider, a login link or a password) get
accounts that are not staff, in `auth.signup_groups`: give those groups the permissions your
app's users need. People who sign up through the admin's login page are staff, as before.

## Tokens

Every login ends in the same answer:

```json
{
  "access_token": "eyJ...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "cadmus_rt_...",
  "refresh_expires_in": 2592000
}
```

Send `access_token` as `Authorization: Bearer <token>`. When it expires (an hour,
`auth.token_lifetime`), trade the refresh token for a new pair:

```js
const res = await fetch("/api/auth/refresh", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ refresh_token }),
});
// 200: store both new tokens. 401: log in again.
```

Each refresh token works **once**. Presenting a spent one again means it was copied, so the
whole login ends, for whoever holds the current token too. Two tabs refreshing with the same
token at the same moment trip this as well: refresh in one place and share the result.

`POST /api/auth/logout` with `{"refresh_token"}` ends the login. A refresh token also stops
working when the password changes, the account is deactivated or deleted, or it goes
`auth.refresh_lifetime` (30 days) without being used. Refresh tokens need the built-in
accounts; set `refresh_lifetime` to zero to turn them off.

## Two-factor login

An account with two-factor login answers the first step with `401` and a challenge:

```json
{ "error": { "code": "totp_required", "message": "..." }, "challenge": "eyJ..." }
```

Ask for the code and finish within 5 minutes:

```js
await fetch("/api/auth/totp", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ challenge, code }), // a code from the app, or a recovery code
});
```

This works the same after every method below. A password login may also send the code with
the password, as `totp`.

## Passwords

```js
await fetch("/api/auth/token", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ username: email, password }),
});
```

The username may be the e-mail address. Attempts are limited as for the admin
([login attempt limits](two-factor.md#login-attempt-limits)).

## Google, Microsoft, GitHub, Facebook and OpenID Connect

The app logs the person in with the provider, then sends what it got to
`POST /api/auth/<name>` (`google`, `microsoft`, `github`, `facebook`, or your
`OidcProvider`'s name). Either:

**An ID token** (Google, Microsoft, any OpenID Connect provider). This is what Google's
sign-in button, NextAuth/Auth.js and the mobile SDKs give you:

```js
// Google Identity Services: the button's callback gets `credential`, an ID token.
async function onGoogleSignIn({ credential }) {
  const res = await fetch("/api/auth/google", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id_token: credential }),
  });
}
```

The server checks the token's signature against the provider's published keys, then its
issuer, audience, expiry and e-mail address. It must be issued to the app's `client_id`, or
to one of `OAuthClient::audiences`: your iOS and Android apps have their own client IDs at
Google, so list them (`GOOGLE_AUDIENCES=ios-id,android-id` with `OAuthClient::from_env`). If
the app sent a `nonce` to the provider, send it too.

Google's button only loads on a page whose address is listed under **Authorized JavaScript
origins** in the OAuth client, such as `https://app.example.com`, and `http://localhost:3000`
while you develop.

**Or the code** the provider sent back to the app, which the server trades using the client
secret. This works with every provider, and is the only way for GitHub and Facebook, which
issue no ID tokens:

```js
// Start: send the browser to the provider with PKCE.
const verifier = randomString(64);
sessionStorage.setItem("pkce", verifier);
location.href = "https://github.com/login/oauth/authorize?" + new URLSearchParams({
  client_id: GITHUB_CLIENT_ID,
  redirect_uri: "https://app.example.com/auth/callback",
  scope: "read:user user:email",
  code_challenge: await sha256base64url(verifier),
  code_challenge_method: "S256",
  state,
});

// Back at /auth/callback?code=...&state=...: check `state`, then
await fetch("/api/auth/github", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({
    code,
    code_verifier: sessionStorage.getItem("pkce"),
    redirect_uri: "https://app.example.com/auth/callback",
  }),
});
```

The `redirect_uri` must be listed in `auth.app.redirect_uris` and registered with the
provider, both exactly. Mobile apps can use their own scheme, such as `myapp://auth`. A
GitHub OAuth app has only one callback URL, so it cannot serve the admin and an app at once;
see [Addresses for apps](login-methods.md#addresses-for-apps) for where each provider keeps
these and what to do about GitHub.

Either way, the account is found as for the admin: the one linked to that identity, or the
one with the address the provider vouches for, or a new account if `signup` allows the
address. Otherwise the answer is `403` with code `no_account`.

## Login links, sign-up and password resets

These e-mail a link that opens **your app**, which sends the token back to the API. Set where
the app is:

```rust
auth: AuthSettings {
    methods: vec![AuthMethod::Password, AuthMethod::MagicLink],
    signup: Signup::Open,
    signup_groups: vec!["Members".into()],
    app: AppSettings {
        url: env_opt("APP_URL"), // e.g. https://app.example.com
        redirect_uris: vec!["https://app.example.com/auth/callback".into()],
        ..Default::default()
    },
    ..Default::default()
},
```

| The app asks | The e-mail links to (default) | The app then sends |
|---|---|---|
| `POST /api/auth/link` `{"email"}` | `/login/link?token=...` | `POST /api/auth/link/verify` `{"token"}`: logged in |
| `POST /api/auth/register` `{"email"}` | `/register/verify?token=...` | `POST /api/auth/register/verify` `{"token", "password"}`: account made, logged in |
| `POST /api/auth/password/forgot` `{"email"}` | `/password/reset?token=...` | `POST /api/auth/password/reset` `{"token", "password"}` |

- The requests answer `202` whether or not the address has an account, so nobody can find
  out who is registered, and they count against the login limits.
- Login links work once, within 15 minutes; sign-up links for a day; reset links for an
  hour, until the password changes.
- Sign-up confirms the address before the account exists, and the password is chosen at the
  end, so it never travels in a link. Only addresses `signup` allows may register.
- Change the paths with `AppSettings::links`; each must contain `{token}`.
- These endpoints are off until `auth.app.url` is set. E-mail is set up as for the admin
  ([E-mail](email.md)).

## The account itself

| Request | What it does |
|---|---|
| `GET /api/auth/me` | who is logged in: username, `staff`, `superuser`, name, `two_factor` |
| `PATCH /api/auth/me` | change `first_name`, `last_name`, `phone` |
| `POST /api/auth/me/password` `{"current", "password"}` | new password; answers with new tokens (older logins end) |
| `POST /api/auth/me/totp` | a new authenticator key: `secret` and an `otpauth://` `uri` for a QR code |
| `POST /api/auth/me/totp/confirm` `{"secret", "code"}` | turns two-factor login on; answers with the recovery codes, once |
| `POST /api/auth/me/totp/disable` `{"code"}` | turns it off, with a code or a recovery code |
| `POST /api/auth/me/recovery-codes` `{"code"}` | a new set of recovery codes |
| `GET /api/auth/me/identities`, `DELETE .../{id}` | the linked sites |
| `GET /api/auth/me/api-keys`, `POST`, `DELETE .../{id}` | the account's [API keys](authentication.md#api-keys) |

Turning two-factor on and making an API key need a login from the last 15 minutes, or the
account's `password` in the request. API keys cannot use these endpoints.

## Logins of your own

To log people in a way of your own (a code by text message, a partner's single sign-on), check
who they are in your route, then hand out a Cadmus login with `Logins`:

```rust
use cadmus::{ApiResult, LoginOutcome, Logins};

async fn sms_login(logins: Logins, Json(body): Json<SmsCode>) -> ApiResult<LoginOutcome> {
    let username = check_sms_code(&body).await?;
    Ok(logins.log_in(&username).await?)
}
```

The answer is the same token pair, or the two-factor challenge for accounts that have it.

## Calling the API from a browser

A front end on another origin needs it listed in `server.cors_origins`. Keep the refresh
token where scripts from other sites cannot read it; the access token can live in memory.

## Related

- [Login methods](login-methods.md): setting up each provider.
- [Logging in - tokens and API keys](authentication.md): the admin's login, and keys for
  programs.
- [Accounts and permissions](accounts.md).
