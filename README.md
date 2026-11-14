# heraapi

[![CI](https://github.com/arnabgupta-dev/heraapi/actions/workflows/ci.yml/badge.svg)](https://github.com/arnabgupta-dev/heraapi/actions/workflows/ci.yml)

A Django-style framework for [axum](https://docs.rs/axum) and
[SeaORM](https://www.sea-ql.org/SeaORM/). Define a model once and get its table, admin pages
and REST endpoints, with authentication, file uploads and management commands built in.

Status: early (0.1). Tested against SQLite and PostgreSQL.

## Quick start

```sh
# installs the `hera` generator
cargo install --git https://github.com/arnabgupta-dev/heraapi heraapi-cli
hera new blog && cd blog
hera generate model Post title:string body:text published:bool views:int rating:float?
cargo run -- migrate                # creates/updates tables from the models
cargo run -- createsuperuser --username admin
cargo run -- serve                  # status page at /, admin at /admin, API docs at /api/docs
```

Generated projects depend on this repository through a Cargo `git` dependency. From a local
checkout, use `cargo install --path cli` and `hera new blog --framework-path /path/to/checkout`.

`hera new` writes `src/settings.rs` (see [Settings](#settings)), a `.env` with the database URL
and a generated signing secret (git-ignored), and a `.env.example`.
`hera new blog --database postgres` (or `mysql`) writes a PostgreSQL `DATABASE_URL` to edit
instead of the SQLite one; every driver is built in, so that address is the only difference.

Field types: `string`, `text`, `int`, `bigint`, `float`, `bool`, `datetime`, `date`, `json`,
and `references` for a foreign key to another model; add `?` for nullable.

A project's `main.rs` is this:

```rust
#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_settings(settings::settings()?).await?;
    models::register(app).routes(routes::router()).run().await
}
```

and registering a model is one line:

```rust
app.model::<post::Entity>(
    ModelOptions::new()
        .label("Posts")
        .list_display(["id", "title", "published"])
        .search_fields(["title"])
        .readonly_fields(["views"])
        .hidden_fields(["internal_note"]),
)
```

## Settings

Like Django's `settings.py`, a project is configured in one file, `src/settings.rs`. It is
plain Rust, so the compiler catches a misspelt setting and your editor completes the names
and shows what each one does. Anything left out keeps its default:

```rust
use heraapi::AdminError;
use heraapi::settings::*;

pub fn settings() -> Result<Settings, AdminError> {
    let debug = env_or("DEBUG", false)?;

    Ok(Settings {
        secret_key: env("SECRET_KEY")?,
        database: DatabaseSettings {
            url: env("DATABASE_URL")?,
            ..Default::default()
        },
        auth: AuthSettings {
            cookie_secure: !debug,
            totp_required: true,
            ..Default::default()
        },
        admin: AdminSettings {
            site_title: "Acme".into(),
            accent_color: "#0a7f5a".into(),
            ..Default::default()
        },
        docs: DocsSettings {
            username: Some("docs".into()),
            password: Some(env("DOCS_PASSWORD")?),
            ..Default::default()
        },
        ..Default::default()
    })
}
```

**Secrets stay in the environment.** The database URL, the signing key and passwords are read
with `env("NAME")?` from `.env` on your machine (loaded automatically) or the host's variables
in production, so they never land in git. `env_or("NAME", default)?` reads an optional value
of any type (`bool` accepts `true/false`, `1/0`, `yes/no`), and `env_opt("NAME")` gives an
`Option<String>`. A missing or malformed variable stops start-up with its name.

**Values are checked at start-up**, before connecting to the database: a mistake stops the
server with the setting it is about, e.g. `admin.accent_color: accent colour must be #rgb or
#rrggbb, got `red``. `cargo run -- check` runs the same checks without serving.

| Section | What it holds |
|---|---|
| `secret_key` | Signs login tokens; at least 32 bytes (`hera secret` prints one) |
| `database` | `url`, `max_connections`, `min_connections`, `acquire_timeout` |
| `server` | `bind` (`127.0.0.1:3000`), `request_timeout` (30 s; `None` for no limit), `cors_origins`, `client_ip_header`, `public_url` |
| `auth` | `methods` (password), `signup`, `signup_groups`, `token_lifetime` (1 h), `totp_required`, `cookie_secure` (`true`), `login_max_attempts` (5), `login_lockout` (5 min) |
| `admin` | `path` (`/admin`), `site_title`, `accent_color`, `logo_url`, `theme`, `custom_css_url`, `footer_text`, `page_size` (25), `nav_links`, `web_fonts`, `template_dir` |
| `api` | `path` (`/api`) |
| `docs` | `enabled`, `username` + `password`, `title`, `version`, `description`, `swagger_js_url`, `swagger_css_url`, `redoc_js_url` |
| `home` | `enabled`, `title`, `tagline` |
| `uploads` | `storage_url` (off until set), `max_file_bytes` (1 GiB), `max_chunk_bytes` (8 MiB) |
| `mail` | `url` (SMTP; unset writes messages to the log), `from` |
| `log_level` | `LogLevel::INFO`; `DEBUG` adds SQL statements |

Durations are written `Duration::from_secs(600)`. Different values per machine are ordinary
code: `bind: env_or("APP_BIND", "127.0.0.1:3000".to_owned())?`, or `if debug { … }`.

What a settings value cannot hold (your own routes added to the API documentation, a custom
account store) is set on the `App` after it is built, e.g.
`App::from_settings(settings()?).await?.docs(DocsConfig::new().extend(...))`.

## The admin

`/admin` is a responsive, mobile-friendly panel with light and dark themes (following the
system, with a toggle). It shows each account exactly what it may do:

- **Dashboard:** every model the account can view, with its row count and the account's
  permissions on it (view, add, change, delete), each spelled out rather than shown by colour
  alone. Superusers also see account totals; everyone sees whether their two-factor
  authentication is on.
- **Navigation:** only the models the account can view; view-only models are marked. Users and
  Groups appear for superusers only.
- **Lists:** search, sorting by clicking a column heading, pagination. On phones each row
  becomes a card.
- **Forms:** buttons appear only for allowed actions, a record the account cannot change opens
  read-only, deleting asks for confirmation, and saving confirms with a short notice.
- **Keyboard:** <kbd>/</kbd> jumps to the search box, and every control is reachable by
  keyboard with a visible focus ring.

The pages load the Fira Sans typeface from Google Fonts; set `admin.web_fonts: false` to use
the system typeface and make no request to another site. The colour, logo, theme and the rest
of the look are set in the `admin` section of [Settings](#settings).

## Home page

`/` is a public status page: the service's name, whether it is operational, whether the
database answers and how fast, and the uptime. It refreshes itself every 30 seconds, and it
links nowhere, so it does not point visitors at the admin or the API documentation.

- **For monitors:** the same URL with `Accept: application/json` returns
  `{"status", "database": {"status", "latency_ms"}, "uptime_secs", "checked_at"}`. Both forms
  answer `503` when the database is unreachable, so a load balancer can use it as a health
  check.
- **API URLs opened in a browser** (typed into the address bar, or followed as a link) are
  sent to `/`. Programs are unaffected: requests with a token, from scripts (`fetch`) and from
  tools like `curl` reach the API as usual. The documentation pages under `/api` are
  exempt.
- **Your own page:** a `/` route in `src/routes.rs` takes precedence automatically. To turn the
  home page off entirely, set `home.enabled: false`; do that too if your routes set their
  own `Router::fallback`.
- `home.title` (default: the site title) and `home.tagline` set what it says.

## Relations

A foreign key is a `references` field. Generate the model it points to first:

```sh
hera generate model Author name:string email:string
hera generate model Post title:string author:references editor:references=author?
```

`author:references` adds an `author_id` column pointing at `author`. `editor:references=author?`
adds an optional `editor_id` that also points at `author`. A model can point to itself
(`parent:references=category?` on `Category`). `migrate` creates the foreign-key constraints.

In the admin, such a field is a dropdown of the related rows on forms, and lists show the
related row's name, linked to it, instead of its number.

- **Which name:** a row is named by its first text column (`name`, `title`, ...). Choose
  another with `ModelOptions::new().display_field("email")`.
- **Long lists:** above 200 related rows there is no dropdown; the form takes the id and says
  which row it currently points to.
- **Permissions:** names are shown only to accounts that may view the related model. Others
  see and edit the plain number.
- **Deleting:** a row that others still point to cannot be deleted (`409`).
- **Hand-written entities:** any `belongs_to` relation in an entity's `Relation` enum is
  picked up the same way; there is nothing to configure.

The REST API is unchanged: it reads and writes the id (`author_id`).

## Management commands

| Command | What it does |
|---|---|
| `serve [--bind addr]` | Start the server (graceful shutdown on Ctrl-C) |
| `migrate` | Create missing tables, columns and unique keys for all models |
| `createsuperuser --username U [--email E] [--password-stdin]` | Create a superuser |
| `promote --username U` | Make an existing account an active superuser |
| `totp --username U [--disable]` | Turn two-factor authentication on (prints a QR code to scan) or off |
| `changepassword --username U [--password-stdin]` | Change a password |
| `openapi` | Print the OpenAPI document as JSON |
| `routes` | List generated URLs |
| `check` | Validate configuration without serving |

`migrate` is additive (SeaORM's entity-first schema sync): it does not drop or rename columns
and keeps no migration history. Use `sea-orm-migration` for destructive changes.

## REST API

For every model, under `/api`:

| Request | Result |
|---|---|
| `GET /api/{model}?page=&per_page=&q=&sort=` | `{"data": [...], "meta": {"total", "page", "per_page"}}` (`per_page` ≤ 100; `sort=title` or `sort=-title` for highest first) |
| `POST /api/{model}` | `201`, `Location`, the created row |
| `GET /api/{model}/{id}` | the row |
| `PUT /api/{model}/{id}` | replace: every required field must be present |
| `PATCH /api/{model}/{id}` | change only the fields sent |
| `DELETE /api/{model}/{id}` | `204` |

Errors are JSON: `{"error": {"code", "message", "field"}}` with `400`, `401`, `403`, `404`,
`409` (unique/foreign-key conflict), `413`, `422` or `429` (too many login attempts). Unknown, hidden and read-only fields in a
body are rejected rather than ignored.

## API documentation

Every project documents its own API, as FastAPI does:

| URL | What it is |
|---|---|
| `/api/docs` | Swagger UI: browse the endpoints and try them out |
| `/api/redoc` | ReDoc: the same as a reference page |
| `/api/openapi.json` | The OpenAPI 3.0 document, for generating clients |

The document is generated from the registered models: every endpoint, the fields that are
returned, the fields that may be sent and which of them are required, the permission each
operation needs, and file uploads if they are enabled. Hidden fields do not appear, and
read-only fields appear only in what is returned.

To call the API from Swagger UI, press **Authorize** and log in with an account's username
and password. Accounts with two-factor authentication get a token from `POST /api/auth/token`
and paste it under `bearer` instead.

**Protecting it.** The documentation is public by default; it reveals model and field names,
never data. To ask for a username and password first (the browser's own prompt), set both in
`src/settings.rs`, with the password in `.env`:

```rust
docs: DocsSettings {
    username: Some("docs".into()),
    password: Some(env("DOCS_PASSWORD")?),
    ..Default::default()
},
```

These only open the documentation; they are not an account and cannot call the API. Guesses
are limited like logins. `enabled: false` removes the documentation entirely.

- **Without the server:** `cargo run -- openapi > openapi.json` prints the document.
- **Your own routes** are not discovered. Add them with
  `app.docs(DocsConfig::new().extend(|doc| { doc["paths"]["/health"] = json!({...}); }))`.
- **Title and version:** `docs.title`, `docs.version`, `docs.description` (Markdown).
- **The viewers load from a CDN** (jsDelivr, pinned versions), so the two pages need internet
  access in the browser. To host them yourself, set `docs.swagger_js_url`,
  `docs.swagger_css_url` and `docs.redoc_js_url`. `openapi.json` needs nothing external.

## Errors

Every failure is an `AdminError`, and one layer decides how it is shown:

| Where the request went | An error looks like |
|---|---|
| under `/api` | JSON, `{"error": {"code", "message", "field"}}` |
| under `/admin` | a page in the admin layout (`error.html`, overridable like the other templates) |
| anywhere else | plain text, or whatever your handler returned |

This includes unknown URLs, wrong methods (`405`) and panics, which become a `500` instead of a
dropped connection. Internal details (SQL, file paths, panic messages) are logged and never
sent to the client.

In your own handlers, return `ApiResult<T>` for JSON errors. Database, storage and I/O errors
convert with `?`, and `AdminError::custom` gives an error your own status and code:

```rust
async fn buy(State(db): State<DatabaseConnection>) -> ApiResult<Json<Receipt>> {
    let item = item::Entity::find_by_id(1).one(&db).await?
        .ok_or_else(|| AdminError::NotFound("item".into()))?;
    if item.left == 0 {
        return Err(AdminError::custom(StatusCode::CONFLICT, "out_of_stock", "sold out").into());
    }
    Ok(Json(Receipt { id: item.id }))
}
```

Log messages go to standard error; `LOG_LEVEL` sets the detail. If your `main` installs its own
`tracing` subscriber before `run()`, that one is used instead.

## Authentication

Built in, not a plugin. Accounts live in the `auth_user` table (`DbUserStore`); implement
`UserStore` to use your own.

- Username or e-mail address + password (Argon2id, verified off the async executor). What an
  account may do once logged in is described under [Permissions](#permissions).
- Accounts have optional details: e-mail (unique, stored in lower case), first and last name
  and phone, plus when they joined and last logged in. Superusers edit them on the Users page;
  `createsuperuser --email` sets the address of the first account. For fields of your own,
  add a model with a `belongs_to` relation to `heraapi::user::Entity` (one profile row per
  account).
- Optional two-factor authentication with any authenticator app (TOTP);
  `auth.totp_required: true` makes it mandatory. See below.
- `POST /api/auth/token` with `{"username", "password", "totp"}` (JSON or a form) returns a
  JWT as `access_token`.
- The API accepts only `Authorization: Bearer <token>`. The admin uses the same JWT in an
  `HttpOnly`, `SameSite=Strict` cookie, and every admin form carries a CSRF token.

### Login methods

Each project picks how people log in, in `src/settings.rs`. The login page shows the
methods in the order given, and only those:

```rust
// Project A: password, or Facebook
auth: AuthSettings {
    methods: vec![
        AuthMethod::Password,
        AuthMethod::Facebook(OAuthClient::from_env("FACEBOOK")?),
    ],
    ..Default::default()
},

// Project B: Google only, and anyone at acme.com gets an account
auth: AuthSettings {
    methods: vec![AuthMethod::Google(OAuthClient::from_env("GOOGLE")?)],
    signup: Signup::Domains(vec!["acme.com".into()]),
    signup_groups: vec!["Staff".into()],
    ..Default::default()
},
```

| Method | What it needs |
|---|---|
| `AuthMethod::Password` | nothing; leave it out to turn password login off everywhere, the API's token endpoint included |
| `AuthMethod::MagicLink` | [e-mail](#e-mail); people type their address and get a login link |
| `AuthMethod::Google(client)` | an OAuth client from the Google Cloud console |
| `AuthMethod::Microsoft { client, tenant }` | an app registration in Microsoft Entra; `tenant` is `common`, `organizations`, `consumers` or your directory's ID |
| `AuthMethod::GitHub(client)` | an OAuth app in GitHub's developer settings |
| `AuthMethod::Facebook(client)` | an app at developers.facebook.com with Facebook Login |
| `AuthMethod::Oidc(OidcProvider::new(name, label, issuer, client))` | any OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, … |

`OAuthClient::from_env("GOOGLE")` reads `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` from
`.env`. Set `server.public_url` to where people reach the site (`PUBLIC_URL` in a generated
project) and register this *redirect URI* with each provider:
`<public_url>/admin/auth/<name>/callback`, e.g.
`https://shop.example.com/admin/auth/google/callback`.

**Who gets in.** A login through another site is matched to the account with the e-mail
address that site vouches for, and the two are linked, so later logins work even if the
address changes. Addresses the site has not verified are never used. Nobody else gets in,
unless `signup` allows it: `Signup::Domains(vec!["acme.com".into()])` or `Signup::Open`
create an account at the first login, as staff without permissions of their own, in
`signup_groups`. The user page lists each account's links, and a superuser can remove one.

- Accounts with two-factor authentication enter their code after coming back, and
  `totp_required` applies as it does to passwords. Inactive accounts cannot log in.
- To start a project without passwords, run `createsuperuser --username you --email
  you@example.com`; it asks for no password and the account logs in through the provider.
- Microsoft's shared directories (`common`, `organizations`, `consumers`) let people choose
  any address, so their addresses only count if the app sends the optional `xms_edov`
  claim. With your own directory's ID as `tenant`, its addresses are trusted.
- GitHub uses the account's primary verified address. For an OpenID Connect provider that
  does not say whether addresses are verified, `trust_email: true` trusts them; only do that
  for a directory whose users cannot pick their own address.
- These logins are for the admin. API clients still get a token from `POST /api/auth/token`.

### E-mail

Login links and password resets are sent by e-mail. Until `mail.url` is set, messages are
written to the server's log instead, so on your own machine you copy the link from the
terminal:

```rust
mail: MailSettings {
    url: env_opt("MAIL_URL"),   // smtps://user:password@smtp.example.com
    from: Some("Acme <noreply@acme.com>".into()),
},
```

`smtps://` (TLS, port 465) and `smtp://…?tls=required` (STARTTLS, port 587) work with any
provider's SMTP service. Unencrypted `smtp://` is accepted only for this machine, e.g. a test
inbox such as Mailpit on `smtp://localhost:1025`. To send through an HTTP API instead,
implement `heraapi::Mailer` (one `send` method) and pass it to `App::mailer`.

- **Login links** (`AuthMethod::MagicLink`): the login page asks for an address and mails a
  link that works once, within 15 minutes; any other login retires it too. Opening the link
  shows a button rather than logging in at once, because mail scanners open links to check
  them. Two-factor accounts then enter their code. `signup` applies as for other sites.
- **Forgot your password?** appears under the password field when passwords are on and
  `server.public_url` is set. The link works for an hour, until the password is changed.
- **Your own password:** the lock button next to your name opens the page to change it.
- **Invitations:** on the new-account form, "Send an invitation instead" leaves the password
  out and e-mails a link, valid once for 7 days, where the person chooses their password and
  is logged in (on sites without passwords, the link just logs them in). The account's page
  has "Send invitation" to send one again.
- The reply to "send me a link" is the same whether or not the address has an account, and
  messages are sent in the background, so neither the answer nor its timing reveals who is
  registered. Requests are limited like login attempts.

### Two-factor authentication

Each user turns it on for themselves: click your name in the admin header, scan the QR code
with an authenticator app, and confirm with your password and a code. From then on, logging
in (the form's code field, or `"totp"` in the token request) needs the current code.

- Each code works once, so a code that was seen or intercepted cannot be reused.
- Turning it off asks for a current code from the app (or a recovery code). Turning it on
  needs the code from the new app, plus the password if the login is more than 15 minutes
  old; accounts without a password log in again instead.
- **Recovery codes:** turning it on shows ten codes, once. Each logs in a single time in
  place of an authenticator code (anywhere a code is asked for, the API included), for when
  the phone is lost. The two-factor page shows how many are left and makes a new set, which
  replaces the old one, after a current code from the app. Only a digest of each code is
  stored.
- Lost device and no recovery codes: a superuser ticks "Turn it off" on the user's page, or
  run `cargo run -- totp --username U --disable`.
- With `auth.totp_required: true` nobody can log in without it, so set up the first account
  from the terminal: `cargo run -- totp --username U` prints the QR code and the recovery
  codes. Users then cannot turn it off themselves.

### Login attempt limits

After 5 login attempts on one account within 5 minutes, further attempts on that account get
`429 Too Many Requests` with a `Retry-After` header until the 5 minutes are over, whether or
not the password is right. A successful login clears the count. One client address gets four
times as many attempts across all accounts. Change the numbers with
`auth.login_max_attempts` (`0` turns the limit off) and `auth.login_lockout`.

- **Behind a reverse proxy**, every request appears to come from the proxy. Set
  `server.client_ip_header` to the header your proxy sets (e.g. `X-Forwarded-For`), but only
  if the proxy always sets it; otherwise clients could choose their own address.
- The counts are kept in memory, per process: several instances each count separately, and a
  restart clears them.
- Someone who knows a username can keep that account locked by guessing. That is the usual
  trade-off of per-account limits; shorten `auth.login_lockout` if it becomes a problem.

## Deploying

A generated project is ready to run behind HTTPS as it is:

- **Time limit:** a request that takes longer than 30 seconds gets `503` (code `timeout`) and
  is abandoned. Change it with `server.request_timeout` (`None` for no limit). File uploads and
  downloads are exempt, and streamed responses are not cut short.
- **Request log:** one line per request with method, path, status and duration, at `info`
  level. The query string is not logged.
- **Protective headers** on every admin and API response: no framing by other sites
  (clickjacking), no content-type guessing, no caching of pages that needed a login, and
  admin URLs are not sent as referrer to other sites.
- **Cross-origin access (CORS)** is off by default. To let a browser app on another origin
  call the API, set `server.cors_origins: vec!["https://app.example.com".into()]` (or `"*"`).
  Such clients use the bearer token; cookies are never accepted across origins.
- **Graceful shutdown** on Ctrl-C.

Before going live, check these:

1. Serve over HTTPS and leave `DEBUG` unset in production: the generated settings turn
   `auth.cookie_secure` off only when `DEBUG=true`.
2. Behind a reverse proxy, set `server.client_ip_header` (see
   [Login attempt limits](#login-attempt-limits)), and `server.bind` to `0.0.0.0:3000` if
   the proxy runs on another machine or container.
3. Set `DATABASE_URL` and `SECRET_KEY` on the host, keep `.env` out of version control, and
   use a different `SECRET_KEY` per environment.

Responses are not compressed, on purpose: admin pages contain a CSRF token next to text the
visitor controls (the search box), which is the situation the BREACH attack on compressed
HTTPS responses needs.

## Permissions

Accounts work as in Django:

| Flag | Meaning |
|---|---|
| Active | Can log in. Untick to lock an account out without deleting it. |
| Staff | Can use the admin and the API. |
| Superuser | Can do everything, including managing accounts. |

Staff who are not superusers can do only what they were given: for each model, any of
**view**, **add**, **change** and **delete**. Permissions are given to an account directly, or
to a **group** that accounts are put in; an account has the sum of both. Whoever may change a
model may also view it. File uploads count as a model named `files`.

Superusers manage all of this in the admin under **Users** and **Groups**. The same rules
apply to the admin and the REST API (`403` with code `forbidden`), and the admin hides the
links and buttons an account cannot use.

- Permissions are read on every request, so a change applies at once, even to tokens already
  issued. Deactivating or deleting an account locks it out immediately.
- Nobody can remove their own access or delete their own account, so there is always a
  superuser left. If you do get locked out, `cargo run -- promote --username U` is the way
  back in.
- Upgrading from a version without superusers: run `migrate`, then `promote` your account.
- With your own `UserStore`, every account is a superuser unless you implement
  `UserStore::access`, and the Users and Groups pages are not shown.

## File uploads

Set `uploads.storage_url` to turn them on. The backend is chosen by URL:

| URL | Backend | Cargo feature |
|---|---|---|
| `file:///absolute/path` | local directory | built in |
| `s3://bucket/prefix` | Amazon S3 and compatibles | `s3` |
| `az://container/prefix` | Azure Blob Storage | `azure` |
| `gs://bucket/prefix` | Google Cloud Storage | `gcs` |

Backend credentials are read from the environment, under the usual names (`AWS_ACCESS_KEY_ID`, `AWS_REGION`,
`AWS_ENDPOINT`, `AZURE_STORAGE_ACCOUNT_NAME`, …).

| Request | Result |
|---|---|
| `POST /api/files` (multipart, field `file`) | streamed to storage; `201` with the file record |
| `GET /api/files`, `GET /api/files/{id}` | metadata |
| `GET /api/files/{id}/content` | streamed download (always as an attachment) |
| `DELETE /api/files/{id}` | `204` |

Resumable chunked uploads:

1. `POST /api/uploads` with `{"filename", "size", "content_type"}` → session `id`.
2. `PATCH /api/uploads/{id}` with header `Upload-Offset: N` and raw bytes, in order. A wrong
   offset gets `409` and the expected offset.
3. `GET /api/uploads/{id}` returns the current offset: this is how a client resumes.
4. The chunk that reaches `size` returns `201` with the file record.
5. `DELETE /api/uploads/{id}` cancels.

Session state is in the database and chunks are in the storage backend, so an upload survives
restarts and can continue on another instance. Storage keys are generated by the server; the
client's filename is kept only as metadata.

In code, `Storage` (`put`, `get`, `delete`, `sink`) and `Uploads` are available for your own
handlers.

## Configuration from the environment only

Projects made before `src/settings.rs` start with `App::from_env()`, which reads everything
from environment variables. It still works; to move to settings, run `hera new` for a fresh
`src/settings.rs`, copy it in, and replace `App::from_env()` with
`App::from_settings(settings::settings()?)`. Rename `ADMIN_JWT_SECRET` to `SECRET_KEY` in
`.env` (or read the old name in settings.rs).

<details>
<summary>The variables <code>App::from_env</code> reads</summary>

| Variable | Meaning | Default |
|---|---|---|
| `DATABASE_URL` | **Required** | — |
| `ADMIN_JWT_SECRET` | **Required.** Signing secret, ≥ 32 bytes (`hera secret`) | — |
| `APP_BIND` | Listen address | `127.0.0.1:3000` |
| `REQUEST_TIMEOUT_SECS` | Longest a request may take; `0` for no limit | `30` |
| `CORS_ALLOWED_ORIGINS` | Origins allowed to call the app from a browser | none |
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug` (adds SQL statements) or `trace` | `info` |
| `ADMIN_BASE_PATH`, `API_BASE_PATH` | URL prefixes | `/admin`, `/api` |
| `DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS`, `DB_ACQUIRE_TIMEOUT_SECS` | Connection pool | driver defaults |
| `ADMIN_JWT_TTL_SECS` | Token lifetime | `3600` |
| `ADMIN_TOTP_REQUIRED` | Refuse accounts without TOTP | `false` |
| `ADMIN_LOGIN_MAX_ATTEMPTS` | Login attempts per account per lockout period; `0` disables | `5` |
| `ADMIN_LOGIN_LOCKOUT_SECS` | Length of the lockout period | `300` |
| `ADMIN_CLIENT_IP_HEADER` | Header carrying the client address behind a proxy | connection address |
| `ADMIN_COOKIE_SECURE` | `false` only for local plain HTTP | `true` |
| `STORAGE_URL` | Enables uploads | off |
| `DOCS_ENABLED` | Serve the API documentation | `true` |
| `DOCS_USERNAME`, `DOCS_PASSWORD` | Ask for these before showing the documentation | open |
| `DOCS_TITLE`, `DOCS_VERSION`, `DOCS_DESCRIPTION` | What the documentation is headed with | site title + "API", `0.1.0` |
| `DOCS_SWAGGER_JS_URL`, `DOCS_SWAGGER_CSS_URL`, `DOCS_REDOC_JS_URL` | Where the documentation pages load their viewer from | jsDelivr |
| `UPLOAD_MAX_FILE_BYTES`, `UPLOAD_MAX_CHUNK_BYTES` | Upload limits | 1 GiB, 8 MiB |
| `ADMIN_SITE_TITLE`, `ADMIN_ACCENT_COLOR`, `ADMIN_LOGO_URL`, `ADMIN_THEME`, `ADMIN_CUSTOM_CSS_URL`, `ADMIN_FOOTER_TEXT`, `ADMIN_PAGE_SIZE`, `ADMIN_NAV_LINKS`, `ADMIN_TEMPLATE_DIR`, `ADMIN_WEB_FONTS` | Admin look and feel | see `UiSettings::from_env` |
| `HOME_PAGE_ENABLED` | Serve the status page at `/` | `true` |
| `HOME_TITLE`, `HOME_TAGLINE` | What the status page says | site title, none |

</details>

## Databases

The address in `database.url` chooses the database, as `DATABASES` does in Django. Every
driver is built in, so switching is a settings change: no code or `Cargo.toml` edit.

| Database | `database.url` |
|---|---|
| SQLite | `sqlite://db.sqlite3?mode=rwc` (`mode=rwc` creates the file) |
| PostgreSQL | `postgres://user:password@host:5432/name` |
| MySQL and MariaDB | `mysql://user:password@host:3306/name` |

Oracle and SQL Server are not supported: open-source SeaORM, the database layer, has no
driver for them.

**Smaller builds.** A project that only ever uses one database can leave the other drivers out
to compile faster: `heraapi = { ..., default-features = false, features = ["postgres"] }`
(the features are `sqlite`, `postgres` and `mysql`). An address for a driver left out then
stops start-up with the feature to turn back on.

The API behaves the same on SQLite and PostgreSQL: date-times are ISO 8601
(`2026-03-04T05:06:07`), JSON columns are returned as JSON, and `?q=` search ignores case and
treats `%` and `_` as ordinary characters.

## Running the tests

```sh
cargo test                       # SQLite, in memory

# the same tests against PostgreSQL (each test uses its own schema)
docker run -d --rm -e POSTGRES_PASSWORD=test -p 5432:5432 postgres:17-alpine
TEST_DATABASE_URL=postgres://postgres:test@localhost:5432/postgres \
  cargo test --features postgres
```

## Not done yet

- **Auth:** passkeys and API keys are planned as further login methods.
  Logins through other sites are for the admin only; API clients cannot use them yet, and
  linking another site to an account goes by e-mail address only (no "connect" button).
  Also missing: per-record (row-level) permissions. Login limits are per process, not shared between instances. Accounts and groups are managed in
  the admin only; there is no REST API for them.
- **Admin:** uploaded files are not editable in the admin; no filters or bulk actions; no
  file-upload field on model forms. Relations: no many-to-many widget, no list of the rows
  that point to a record, and no search-as-you-type for long lists of related rows.
- **Models:** single-column primary keys only; UUID, decimal, enum and array columns are
  read-only.
- **Uploads:** completing a resumable upload copies the chunks once inside the storage backend
  (no direct-to-S3 multipart yet); abandoned sessions are not cleaned up automatically.
- **Server:** no response compression (see [Deploying](#deploying)), request ids or metrics;
  add them with your own `tower` layers.
- **API documentation:** your own routes are not discovered automatically, and the viewers are
  loaded from a CDN rather than bundled.
- **Verification:** MySQL, S3, Azure and GCS compile but have not been run against real
  services.

## License

[MIT](LICENSE). Free to use and modify, commercially or otherwise. Copies must keep the
copyright notice; beyond that, a link back to
[this repository](https://github.com/arnabgupta-dev/heraapi) wherever you use it is
appreciated.
