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
cargo run -- serve                  # http://127.0.0.1:3000/admin
```

Generated projects depend on this repository through a Cargo `git` dependency. From a local
checkout, use `cargo install --path cli` and `hera new blog --framework-path /path/to/checkout`.

`hera new` writes a `.env` with a generated signing secret (git-ignored) and a `.env.example`.
`hera new blog --database postgres` (or `mysql`) sets the project up for that database instead
of SQLite: it selects the driver and writes a matching `DATABASE_URL` to edit.

Field types: `string`, `text`, `int`, `bigint`, `float`, `bool`, `datetime`, `date`, `json`,
and `references` for a foreign key to another model; add `?` for nullable.

A project's `main.rs` is this:

```rust
#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_env().await?;
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
| `createsuperuser --username U [--password-stdin]` | Create a superuser |
| `promote --username U` | Make an existing account an active superuser |
| `totp --username U [--disable]` | Turn two-factor authentication on (prints a QR code to scan) or off |
| `changepassword --username U [--password-stdin]` | Change a password |
| `routes` | List generated URLs |
| `check` | Validate configuration without serving |

`migrate` is additive (SeaORM's entity-first schema sync): it does not drop or rename columns
and keeps no migration history. Use `sea-orm-migration` for destructive changes.

## REST API

For every model, under `/api`:

| Request | Result |
|---|---|
| `GET /api/{model}?page=&per_page=&q=` | `{"data": [...], "meta": {"total", "page", "per_page"}}` (`per_page` ≤ 100) |
| `POST /api/{model}` | `201`, `Location`, the created row |
| `GET /api/{model}/{id}` | the row |
| `PUT /api/{model}/{id}` | replace: every required field must be present |
| `PATCH /api/{model}/{id}` | change only the fields sent |
| `DELETE /api/{model}/{id}` | `204` |

Errors are JSON: `{"error": {"code", "message", "field"}}` with `400`, `401`, `403`, `404`,
`409` (unique/foreign-key conflict), `413`, `422` or `429` (too many login attempts). Unknown, hidden and read-only fields in a
body are rejected rather than ignored.

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

- Username + password (Argon2id, verified off the async executor). What an account may do
  once logged in is described under [Permissions](#permissions).
- Optional two-factor authentication with any authenticator app (TOTP);
  `ADMIN_TOTP_REQUIRED=true` makes it mandatory. See below.
- `POST /api/auth/token` with `{"username", "password", "totp"}` returns a JWT.
- The API accepts only `Authorization: Bearer <token>`. The admin uses the same JWT in an
  `HttpOnly`, `SameSite=Strict` cookie, and every admin form carries a CSRF token.

### Two-factor authentication

Each user turns it on for themselves: click your name in the admin header, scan the QR code
with an authenticator app, and confirm with your password and a code. From then on, logging
in (the form's code field, or `"totp"` in the token request) needs the current code.

- Each code works once, so a code that was seen or intercepted cannot be reused.
- Turning it on or off asks for the password again.
- Lost device: a superuser ticks "Turn it off" on the user's page, or run
  `cargo run -- totp --username U --disable`.
- With `ADMIN_TOTP_REQUIRED=true` nobody can log in without it, so set up the first account
  from the terminal: `cargo run -- totp --username U` prints the QR code. Users then cannot
  turn it off themselves.

### Login attempt limits

After 5 login attempts on one account within 5 minutes, further attempts on that account get
`429 Too Many Requests` with a `Retry-After` header until the 5 minutes are over, whether or
not the password is right. A successful login clears the count. One client address gets four
times as many attempts across all accounts. Change the numbers with
`ADMIN_LOGIN_MAX_ATTEMPTS` (`0` turns the limit off) and `ADMIN_LOGIN_LOCKOUT_SECS`.

- **Behind a reverse proxy**, every request appears to come from the proxy. Set
  `ADMIN_CLIENT_IP_HEADER` to the header your proxy sets (e.g. `X-Forwarded-For`), but only
  if the proxy always sets it; otherwise clients could choose their own address.
- The counts are kept in memory, per process: several instances each count separately, and a
  restart clears them.
- Someone who knows a username can keep that account locked by guessing. That is the usual
  trade-off of per-account limits; lower `ADMIN_LOGIN_LOCKOUT_SECS` if it becomes a problem.

## Deploying

A generated project is ready to run behind HTTPS as it is:

- **Time limit:** a request that takes longer than 30 seconds gets `503` (code `timeout`) and
  is abandoned. Change it with `REQUEST_TIMEOUT_SECS` (`0` for no limit). File uploads and
  downloads are exempt, and streamed responses are not cut short.
- **Request log:** one line per request with method, path, status and duration, at `info`
  level. The query string is not logged.
- **Protective headers** on every admin and API response: no framing by other sites
  (clickjacking), no content-type guessing, no caching of pages that needed a login, and
  admin URLs are not sent as referrer to other sites.
- **Cross-origin access (CORS)** is off by default. To let a browser app on another origin
  call the API, set `CORS_ALLOWED_ORIGINS=https://app.example.com` (comma-separated, or `*`).
  Such clients use the bearer token; cookies are never accepted across origins.
- **Graceful shutdown** on Ctrl-C.

Before going live, check these:

1. Serve over HTTPS and remove `ADMIN_COOKIE_SECURE=false` from `.env` (it is there for local
   development only).
2. Behind a reverse proxy, set `ADMIN_CLIENT_IP_HEADER` (see
   [Login attempt limits](#login-attempt-limits)).
3. Keep `.env` out of version control, and use a different `ADMIN_JWT_SECRET` per environment.

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

Set `STORAGE_URL` to turn them on. The backend is chosen by URL:

| URL | Backend | Cargo feature |
|---|---|---|
| `file:///absolute/path` | local directory | built in |
| `s3://bucket/prefix` | Amazon S3 and compatibles | `s3` |
| `az://container/prefix` | Azure Blob Storage | `azure` |
| `gs://bucket/prefix` | Google Cloud Storage | `gcs` |

Backend credentials use the usual variables (`AWS_ACCESS_KEY_ID`, `AWS_REGION`,
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

## Configuration (environment or `.env`)

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
| `UPLOAD_MAX_FILE_BYTES`, `UPLOAD_MAX_CHUNK_BYTES` | Upload limits | 1 GiB, 8 MiB |
| `ADMIN_SITE_TITLE`, `ADMIN_ACCENT_COLOR`, `ADMIN_LOGO_URL`, `ADMIN_THEME`, `ADMIN_CUSTOM_CSS_URL`, `ADMIN_FOOTER_TEXT`, `ADMIN_PAGE_SIZE`, `ADMIN_NAV_LINKS`, `ADMIN_TEMPLATE_DIR` | Admin look and feel | see `UiSettings::from_env` |

Database drivers are Cargo features: `sqlite` (default), `postgres`, `mysql`. `hera new
--database` picks one for a new project; to switch an existing project, change the dependency
to `heraapi = { ..., default-features = false, features = ["postgres"] }` and its
`DATABASE_URL`.

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

- **Auth:** no OIDC/OAuth login, API keys, per-record (row-level) permissions, recovery codes
  for two-factor authentication, or a page for users to change their own password. Login
  limits are per process, not shared between instances. Accounts and groups are managed in
  the admin only; there is no REST API for them.
- **Admin:** uploaded files are not editable in the admin; no filters or bulk actions; no
  file-upload field on model forms. Relations: no many-to-many widget, no list of the rows
  that point to a record, and no search-as-you-type for long lists of related rows.
- **Models:** single-column primary keys only; UUID, decimal, enum and array columns are
  read-only.
- **Uploads:** completing a resumable upload copies the chunks once inside the storage backend
  (no direct-to-S3 multipart yet); abandoned sessions are not cleaned up automatically.
- **Server:** no response compression (see [Deploying](#deploying)), request ids or metrics;
  add them with your own `tower` layers. No OpenAPI document.
- **Verification:** MySQL, S3, Azure and GCS compile but have not been run against real
  services.

## License

[MIT](LICENSE). Free to use and modify, commercially or otherwise. Copies must keep the
copyright notice; beyond that, a link back to
[this repository](https://github.com/arnabgupta-dev/heraapi) wherever you use it is
appreciated.
