---
title: Settings
section: Guides
order: 150
summary: Configuring a project in src/settings.rs - secrets from the environment, checks at start-up, and every setting with its default.
---

A project is configured in one file, `src/settings.rs`. It is plain Rust, so the compiler
catches a misspelt setting, and your editor completes the names and shows what each one
does. Anything left out keeps its default.

```rust
use cadmus::AdminError;
use cadmus::settings::*;

pub fn settings() -> Result<Settings, AdminError> {
    let debug = env_or("DEBUG", false)?;

    Ok(Settings {
        secret_key: env("SECRET_KEY")?,
        database: DatabaseSettings {
            url: env("DATABASE_URL")?,
            ..Default::default()
        },
        auth: AuthSettings {
            cookie_secure: !debug,     // the login cookie needs HTTPS, except on your machine
            ..Default::default()
        },
        admin: AdminSettings {
            site_title: "Acme".into(),
            accent_color: "#0a7f5a".into(),
            ..Default::default()
        },
        ..Default::default()
    })
}
```

## Secrets stay out of the code

The database address, the secret key and passwords are read from the environment: from
`.env` on your machine (loaded automatically), from the server's environment variables in
production.

| Helper | Reads |
|---|---|
| `env("NAME")?` | a required value; start-up stops, naming it, if it is missing |
| `env_or("NAME", default)?` | an optional value of any type (`true/false`, `1/0`, `yes/no` for yes-or-no) |
| `env_opt("NAME")` | optional text, as `Option<String>` |

Values that differ per machine are ordinary code: `if debug { ... }`, or
`bind: env_or("APP_BIND", "127.0.0.1:3000".to_owned())?`.

## Checked at start-up

A mistake stops the server with the setting it is about, before it connects to the
database, for example:

```text
admin.accent_color: accent colour must be #rgb or #rrggbb, got `red`
```

`cargo run -- check` runs the same checks without starting the server.

## Every setting

| Section | Settings (defaults in brackets) |
|---|---|
| `secret_key` | signs logins; at least 32 bytes (`cadmus secret` prints one) |
| `database` | `url`, `max_connections`, `min_connections`, `acquire_timeout` |
| `server` | `bind` (`127.0.0.1:3000`), `request_timeout` (30 s; `None` for no limit), `cors_origins`, `client_ip_header`, `public_url`, `hsts` (a year; `None` to not send it), `compress_api` (`true`) |
| `auth` | `methods` (password), `signup`, `signup_groups`, `token_lifetime` (1 h), `refresh_lifetime` (30 days), `totp_required`, `cookie_secure` (`true`), `login_max_attempts` (5), `login_lockout` (5 min), `api_keys` (`true`), `app` (`url`, `redirect_uris`, `links`: see [Logins for apps](app-logins.md)) |
| `admin` | `path` (`/admin`), `site_title`, `accent_color`, `logo_url`, `theme`, `custom_css_url`, `footer_text`, `page_size` (25), `nav_links`, `web_fonts`, `template_dir` |
| `api` | `path` (`/api`) |
| `docs` | `enabled`, `username` + `password`, `title`, `version`, `description`, `swagger_js_url`, `swagger_css_url`, `redoc_js_url` |
| `home` | `enabled`, `title`, `tagline` |
| `uploads` | `storage_url` (off until set), `max_file_bytes` (1 GiB), `max_chunk_bytes` (8 MiB) |
| `mail` | `url` (SMTP; unset writes messages to the log), `from` |
| `log_level` | `LogLevel::INFO`; `DEBUG` also shows every SQL statement |
| `rate_limit` | `enabled` (`true`), `anonymous` (120 a minute per address), `logged_in` (600 a minute per account or API key), `admin` (`None`: not limited), `rules`: see [Rate limits](rate-limits.md) |

Durations are written `Duration::from_secs(600)`.

## What settings cannot hold

Things that are code rather than values are set on the `App` after it is built:

```rust
App::from_settings(settings::settings()?)
    .await?
    .docs(DocsConfig::new().extend(|doc| { /* your routes in the API documentation */ }))
    .mailer(MyMailer::new())          // your own way of sending e-mail
    .users(MyUserStore::new())        // accounts kept elsewhere
```

## Home page

`/` is a public status page: the service's name, whether it is up, whether the database
answers and how fast, and how long it has been running. It refreshes every 30 seconds and
links nowhere.

- With `Accept: application/json` it returns
  `{"status", "database": {"status", "latency_ms"}, "uptime_secs", "checked_at"}`. Both forms
  answer `503` when the database is down, so a load balancer can use it as a health check.
- A `/` route in `src/routes.rs` replaces it automatically. `home.enabled: false` turns it
  off; `home.title` and `home.tagline` set what it says.
- API addresses typed into a browser's address bar are sent to `/`; programs reach the API
  as usual.
