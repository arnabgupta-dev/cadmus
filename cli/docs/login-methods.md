---
title: Login methods
section: Guides
order: 100
summary: Passwords, login links by e-mail, and logging in with Google, Microsoft, GitHub, Facebook or any OpenID Connect provider.
---

Each project chooses how people log in, in the `auth` section of `src/settings.rs`. The
login page shows the chosen methods, in order, and only those.

```rust
// Password, or Facebook
auth: AuthSettings {
    methods: vec![
        AuthMethod::Password,
        AuthMethod::Facebook(OAuthClient::from_env("FACEBOOK")?),
    ],
    ..Default::default()
},
```

```rust
// Google only, and anyone at acme.com gets an account on first login
auth: AuthSettings {
    methods: vec![AuthMethod::Google(OAuthClient::from_env("GOOGLE")?)],
    signup: Signup::Domains(vec!["acme.com".into()]),
    signup_groups: vec!["Staff".into()],
    ..Default::default()
},
```

## The methods

| Method | What it needs |
|---|---|
| `AuthMethod::Password` | nothing; leave it out to turn passwords off everywhere, the API's token endpoint included |
| `AuthMethod::MagicLink` | [e-mail](email.md); people type their address and get a login link |
| `AuthMethod::Google(client)` | an OAuth client from the Google Cloud console |
| `AuthMethod::Microsoft { client, tenant }` | an app registration in Microsoft Entra; `tenant` is `common`, `organizations`, `consumers` or your directory's ID |
| `AuthMethod::GitHub(client)` | an OAuth app in GitHub's developer settings |
| `AuthMethod::Facebook(client)` | an app at developers.facebook.com with Facebook Login |
| `AuthMethod::Oidc(OidcProvider::new(name, label, issuer, client))` | any OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, ... |

## Setting up a provider

1. Register an app with the provider. Give it this *redirect URI*:
   `<public_url>/admin/auth/<name>/callback`, for example
   `https://shop.example.com/admin/auth/google/callback`.
2. Put its client ID and secret in `.env`: `OAuthClient::from_env("GOOGLE")` reads
   `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET`.
3. Set `server.public_url` to the site's address (`PUBLIC_URL` in a generated project).

### Addresses for apps

When a web or mobile app logs its users in through the API ([Logins for apps](app-logins.md)),
the provider must also know the app's addresses:

- **A code flow** (every provider): register the app's own redirect URI, such as
  `https://app.example.com/auth/callback` or `myapp://auth`, next to the admin's, and list it
  in `auth.app.redirect_uris`. Providers compare it exactly, character for character.
- **An ID token from the browser** (Google's sign-in button, an OpenID Connect provider's
  JavaScript library): no redirect URI, but the provider must allow the page's origin.

Where each provider keeps these:

| Provider | Redirect URIs | For an ID token in the browser |
|---|---|---|
| Google | **Authorized redirect URIs** of the OAuth client; several allowed | the page's address, such as `https://app.example.com` or `http://localhost:3000`, under **Authorized JavaScript origins**. Without it the sign-in button does not load. |
| Microsoft | **Redirect URIs** of the app registration, under the **Web** platform (the server redeems the code with the secret); several allowed | the page's address under the **Single-page application** platform |
| GitHub | an OAuth app has **one** callback URL, and the app's `redirect_uri` must match it exactly (a subdirectory of it was refused in testing) | GitHub issues no ID tokens; use the code |
| Facebook | **Valid OAuth Redirect URIs** under Facebook Login; several allowed | Facebook issues no ID tokens; use the code |
| OpenID Connect | the client's valid redirect URIs (Keycloak: **Valid redirect URIs**) | the client's allowed web origins, and whichever flow the library uses (Keycloak's **Implicit flow** for `response_type=id_token`) |

Because a GitHub OAuth app has a single callback URL, the admin and an app cannot both log in
with the same one. Either register a **GitHub App** instead, which accepts several callback
URLs (give it read access to the account's **Email addresses** so the address can be
checked), or use GitHub only for the app or only for the admin.

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
get into the admin, if only with the groups' permissions. Each account's page lists its links to other sites, and a
superuser can remove one.

## Details

- Accounts with two-factor login enter their code after coming back from the other site,
  and `totp_required` applies as for passwords. Inactive accounts cannot log in.
- To start a project without passwords, run
  `cargo run -- createsuperuser --username you --email you@example.com`: it asks for no
  password, and the account logs in through the provider.
- Microsoft's shared directories (`common`, `organizations`, `consumers`) let people choose
  any address, so their addresses only count if the app sends the optional `xms_edov` claim.
  With your own directory's ID as `tenant`, its addresses are trusted.
- GitHub uses the account's primary verified address. For an OpenID Connect provider that
  does not say whether addresses are verified, `trust_email: true` trusts them; only do that
  when users cannot pick their own address.
- The same methods work for apps through the REST API: the app logs in with the provider
  and sends the ID token or the code to `POST /api/auth/<name>`; register the app's
  addresses with the provider too ([Addresses for apps](#addresses-for-apps)). See
  [Logins for apps](app-logins.md). Programs use an [API key](authentication.md).
