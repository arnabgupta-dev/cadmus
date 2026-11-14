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

New accounts are staff with no permissions of their own, in the `signup_groups`, so those
groups decide what they may do. Each account's page lists its links to other sites, and a
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
- These logins are for the admin. Programs use an [API key](authentication.md).
