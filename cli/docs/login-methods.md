---
title: Login methods
section: Guides
order: 100
summary: Passwords, login links by e-mail, and logging in with Google, Microsoft, GitHub, Facebook or any OpenID Connect provider, and the addresses to register with each.
---

Each project chooses how people log in. The login page, and the REST API's login endpoints,
offer the chosen methods, in order, and only those.

## Adding a method

Passwords are always built in. Every other method is a Cargo feature, so a project compiles
only the ones it uses:

```sh
cadmus add google github      # or: cadmus new blog --features google,github
```

That turns the features on in `Cargo.toml`, adds the methods to `methods()` in
`src/settings.rs`, and adds their keys to `.env`:

```rust
fn methods() -> Result<Vec<AuthMethod>, AdminError> {
    let mut methods = Vec::new();
    methods.push(AuthMethod::Password);
    if let Some(client) = OAuthClient::from_env_if_set("GOOGLE")? {
        methods.push(AuthMethod::Google(client));
    }
    if let Some(client) = OAuthClient::from_env_if_set("GITHUB")? {
        methods.push(AuthMethod::GitHub(client));
    }
    // cadmus:methods (login methods are added above this line)
    Ok(methods)
}
```

A site whose keys are not in `.env` yet is left out, so the project runs before you have
registered it. Remove a line to turn a method off (`AuthMethod::Password` included, which
also turns off the API's password endpoint).

| Method | Feature | What it needs |
|---|---|---|
| `AuthMethod::Password` | always there | nothing |
| `AuthMethod::MagicLink` | `magic-link` | [e-mail](email.md); people type their address and get a login link |
| `AuthMethod::Google(client)` | `google` | an OAuth client in the Google Cloud console |
| `AuthMethod::Microsoft { client, tenant }` | `microsoft` | an app registration in Microsoft Entra; `tenant` is `common`, `organizations`, `consumers` or your directory's ID |
| `AuthMethod::GitHub(client)` | `github` | an OAuth app (or a GitHub App) in GitHub's developer settings |
| `AuthMethod::Facebook(client)` | `facebook` | an app at developers.facebook.com with Facebook Login |
| `AuthMethod::Oidc(OidcProvider::new(name, label, issuer, client))` | `oidc` | any OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, ... |

A method listed without its feature stops the server at startup with the line to add, for
example `` `github` needs cadmus's `github` feature ... (or run `cadmus add github`) ``.

## The addresses to register

Logging in through another site sends the browser there and back. The address it comes back
to, the **redirect URI**, must be registered with the provider, and it depends on who
receives the person back:

- **The admin**: the Cadmus server receives them at
  `<public_url>/admin/auth/<name>/callback`, trades the code and logs them in to the admin.
- **Your own app** (React, Next.js, a mobile app): your app receives them at one of its own
  pages, such as `https://app.example.com/auth/callback`, and sends what it got to
  `POST /api/auth/<name>` ([Logins for apps](app-logins.md)). List that address in
  `auth.app.redirect_uris` too.
- **An ID token in the browser** (Google's sign-in button, an OpenID Connect provider's
  JavaScript library) needs no redirect URI, but the provider must allow the page's address,
  its *origin*.

The admin and your app are different programs, usually on different addresses, so a site
used by both needs both registered. For a shop whose admin and API are at
`https://api.shop.com` and whose React app is at `https://shop.com`:

| Register with the provider | For |
|---|---|
| `https://api.shop.com/admin/auth/google/callback` | the admin |
| `https://shop.com/auth/callback` | your app's code flow (also in `auth.app.redirect_uris`) |
| `https://shop.com` as an allowed origin | Google's sign-in button on your app |

and on your machine, with Cadmus on port 3000 and a Vite app on 5173:

| Register with the provider | For |
|---|---|
| `http://localhost:3000/admin/auth/google/callback` | the admin |
| `http://localhost:5173/auth/callback` | your app's code flow |
| `http://localhost:5173` as an allowed origin | a sign-in button on your app |

Providers compare addresses exactly, character for character: `localhost` is not
`127.0.0.1`, and a trailing `/` matters. Set `server.public_url` to the site's address
(`PUBLIC_URL` in `.env`), and put each app's client ID and secret in `.env`
(`GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`, and so on).

## Each provider

**Google** (Google Cloud console, APIs & Services, Credentials, an OAuth client ID of type
*Web application*):

- **Authorized redirect URIs**: the addresses above; several are allowed.
- **Authorized JavaScript origins**: your app's address, for the sign-in button (without it
  the button does not load).
- The consent screen needs the `email`, `profile` and `openid` scopes, which are the
  defaults. iOS and Android apps have client IDs of their own: list them in
  `GOOGLE_AUDIENCES` so their ID tokens are accepted.

**Microsoft** (Microsoft Entra admin center, App registrations; registering an app needs an
Entra directory):

- **Supported account types**: who may log in. Match it with `tenant`: `common` for work,
  school and personal accounts, `organizations`, `consumers`, or your directory's ID.
- **Redirect URIs** under the **Web** platform (the server trades the code with the secret);
  several are allowed. An app using a JavaScript library to get ID tokens adds its address
  under the **Single-page application** platform.
- **Certificates & secrets**: a client secret; its *Value* goes in `MICROSOFT_CLIENT_SECRET`.
- **Token configuration**: add the optional claims `email` and `xms_edov` to the ID token.
  With the shared directories (`common`, `organizations`, `consumers`) people can choose any
  address, so an address only counts when `xms_edov` says its domain is verified; without
  it, those people are refused. With your own directory as `tenant`, its addresses are
  trusted as they are.

**GitHub** (Settings, Developer settings):

- An **OAuth app**: add each address as an **Authorization callback URL**; several are
  allowed, so one app serves the admin and your app.
- A **GitHub App** works too: give it read access to the account's **Email addresses**.
- GitHub issues no ID tokens; apps send the code. The account's primary verified address is
  used.

**Facebook** (developers.facebook.com, Create app, the use case "Authenticate and request
data from users with Facebook Login"):

- Add the **email** permission; without it Facebook shares no address and people are
  refused unless their Facebook account was linked before.
- **Valid OAuth Redirect URIs** under Facebook Login's settings; several are allowed.
- While the app is in **Development** mode, only people with a role on it can log in, and
  `localhost` addresses are allowed. Going live needs Facebook's review.
- Facebook issues no ID tokens; apps send the code.

**Any OpenID Connect provider** (Okta, Auth0, Keycloak, GitLab, ...):

- The client's redirect URIs (Keycloak: **Valid redirect URIs**) and allowed web origins.
- For ID tokens straight from a browser library, the flow it uses must be allowed
  (Keycloak: **Implicit flow** for `response_type=id_token`).
- `OidcProvider::new("okta", "Okta", "https://acme.okta.com", client)`: the issuer is the
  address its discovery document lives under. For a provider that does not say whether
  addresses are verified, setting the provider's `trust_email` field to `true` trusts them;
  only do that when users cannot pick their own address.

## Who gets in

A login through another site is matched to the account with the e-mail address that site
vouches for, and the two are then linked, so later logins work even if the address changes.
Addresses the site has not verified are never used.

Nobody else gets in unless `signup` allows it:

| `signup` | Who may get an account at their first login |
|---|---|
| `Signup::Closed` (default) | nobody: only existing accounts |
| `Signup::Domains(vec!["acme.com".into()])` | people with an address at those domains |
| `Signup::Open` | anyone the provider vouches for |

New accounts have no permissions of their own and are put in the `signup_groups`, so those
groups decide what they may do. Accounts made at the admin's login page are staff; accounts
made through the API are not, and can only use the API (see
[Logins for apps](app-logins.md#who-may-log-in)). With `Signup::Open`, anyone can therefore
get into the admin, if only with the groups' permissions. Each account's page lists its links
to other sites, and a superuser can remove one.

## Details

- Accounts with two-factor login enter their code after coming back from the other site,
  and `totp_required` applies as for passwords. Inactive accounts cannot log in.
- To start a project without passwords, run
  `cargo run -- createsuperuser --username you --email you@example.com`: it asks for no
  password, and the account logs in through the provider.
- Programs use an [API key](authentication.md), not a login method.
