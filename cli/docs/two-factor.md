---
title: Two-factor login and limits
section: Guides
order: 110
summary: Codes from a phone app, recovery codes, making two-factor compulsory, and limits on login attempts.
---

Two-factor login asks for a code from an app on your phone (Google Authenticator, 1Password,
Aegis, ...) as well as the password, so a stolen password alone is not enough.

## Turning it on

Each person turns it on for themselves: click your name in the admin menu, scan the QR code
with the app, and confirm with a code. From then on, logging in needs the current code: in
the login form, or as `"totp"` in an [API token request](authentication.md).

- Each code works once, so an intercepted code cannot be reused.
- Turning it on needs the password too if the login is more than 15 minutes old; accounts
  without a password log out and in again instead.
- Turning it off asks for a current code from the app, or a recovery code.

## Recovery codes

Turning it on shows ten recovery codes, once. Each logs in a single time in place of a phone
code (anywhere a code is asked for, the API included), for when the phone is lost. The
two-factor page shows how many are left, and makes a new set (replacing the old one) after a
current code from the app. Only a fingerprint of each code is stored.

## Lost phone, no recovery codes

A superuser ticks "Turn it off" on the person's account page, or run:

```sh
cargo run -- totp --username U --disable
```

## Making it compulsory

```rust
auth: AuthSettings {
    totp_required: true,
    ..Default::default()
},
```

Nobody can then log in without it, so set up the first account from the terminal:
`cargo run -- totp --username U` prints the QR code and the recovery codes. People then
cannot turn it off themselves.

## Login attempt limits

To stop password guessing: after 5 failed attempts on one account within 5 minutes, further
attempts on that account are refused (`429 Too Many Requests`, with a `Retry-After` header)
until the 5 minutes are over. A successful login clears the count. One client address gets
four times as many attempts across all accounts.

Change the numbers with `auth.login_max_attempts` (`0` turns the limit off) and
`auth.login_lockout`.

- **Behind a reverse proxy** (nginx, a load balancer), every request seems to come from the
  proxy. Set `server.client_ip_header` to the header the proxy sets (e.g.
  `X-Forwarded-For`), but only if it always sets it; otherwise clients could pretend to be
  anyone.
- Counts are kept in memory, per running copy of the server: several copies each count
  separately, and a restart clears them.
- Someone who knows a username can keep that account locked by guessing. That is the usual
  trade-off of per-account limits; shorten `auth.login_lockout` if it becomes a problem.
