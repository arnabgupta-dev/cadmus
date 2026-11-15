---
title: E-mail
section: Guides
order: 120
summary: Sending login links, password resets and invitations, over SMTP or your own mail service.
---

Login links, password resets and invitations are sent by e-mail. Until `mail.url` is set,
messages are written to the server's log instead, so on your own machine you copy the link
from the terminal.

## Setting it up

Sending over SMTP is the `smtp` feature: `cadmus add smtp` (or `features = ["smtp"]` on the
`cadmus` dependency). Without it, messages always go to the log, and setting `mail.url`
stops the server at startup with the line to add.

```rust
mail: MailSettings {
    url: env_opt("MAIL_URL"),   // smtps://user:password@smtp.example.com
    from: Some("Acme <noreply@acme.com>".into()),
},
```

| `MAIL_URL` | Connection |
|---|---|
| `smtps://user:password@host` | TLS from the start, port 465 |
| `smtp://user:password@host:587?tls=required` | STARTTLS, port 587 |
| `smtp://localhost:1025` | unencrypted, accepted only to this machine (a test inbox such as Mailpit) |

Any provider with an SMTP service works. To send through an HTTP API instead, implement
`cadmus::Mailer` (one `send` method) and pass it to `App::mailer`.

## What is sent

- **Login links** (`AuthMethod::MagicLink`, the `magic-link` feature): the login page asks for an address and mails a
  link that works once, within 15 minutes. Opening it shows a button rather than logging in
  straight away, because mail scanners open links to check them.
- **Forgot your password?** appears under the password field when passwords are on and
  `server.public_url` is set. The link works for an hour.
- **Invitations:** on the new-account form, "Send an invitation instead" e-mails a link,
  valid once for 7 days, where the person chooses their password (on sites without
  passwords, the link just logs them in). The account's page can send it again.

The answer to "send me a link" is the same whether or not the address has an account, and
mail is sent in the background, so nobody can find out who is registered. Requests are
limited like login attempts.
