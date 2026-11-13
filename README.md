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

Field types: `string`, `text`, `int`, `bigint`, `float`, `bool`, `datetime`, `date`, `json`;
add `?` for nullable.

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

## Management commands

| Command | What it does |
|---|---|
| `serve [--bind addr]` | Start the server (graceful shutdown on Ctrl-C) |
| `migrate` | Create missing tables, columns and unique keys for all models |
| `createsuperuser --username U [--password-stdin]` | Create an account |
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
`409` (unique/foreign-key conflict), `413` or `422`. Unknown, hidden and read-only fields in a
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

- Username + password (Argon2id, verified off the async executor).
- TOTP second factor for accounts with a secret; `ADMIN_TOTP_REQUIRED=true` makes it mandatory.
- `POST /api/auth/token` with `{"username", "password", "totp"}` returns a JWT.
- The API accepts only `Authorization: Bearer <token>`. The admin uses the same JWT in an
  `HttpOnly`, `SameSite=Strict` cookie, and every admin form carries a CSRF token.

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
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug` (adds SQL statements) or `trace` | `info` |
| `ADMIN_BASE_PATH`, `API_BASE_PATH` | URL prefixes | `/admin`, `/api` |
| `DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS`, `DB_ACQUIRE_TIMEOUT_SECS` | Connection pool | driver defaults |
| `ADMIN_JWT_TTL_SECS` | Token lifetime | `3600` |
| `ADMIN_TOTP_REQUIRED` | Refuse accounts without TOTP | `false` |
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

- **Auth:** no OIDC/OAuth login, API keys, per-user or per-model permissions (any staff account
  can do everything), login rate limiting, token revocation or TOTP replay protection. No
  command or page to enrol a TOTP secret.
- **Admin:** accounts and uploaded files are not editable in the admin; no relation widgets,
  filters or bulk actions; no file-upload field on model forms.
- **Models:** single-column primary keys only; UUID, decimal, enum and array columns are
  read-only.
- **Uploads:** completing a resumable upload copies the chunks once inside the storage backend
  (no direct-to-S3 multipart yet); abandoned sessions are not cleaned up automatically.
- **Server:** no request timeout, compression, CORS or request-logging layers by default; add
  them with your own `tower` layers. No OpenAPI document.
- **Verification:** MySQL, S3, Azure and GCS compile but have not been run against real
  services.

## License

[MIT](LICENSE). Free to use and modify, commercially or otherwise. Copies must keep the
copyright notice; beyond that, a link back to
[this repository](https://github.com/arnabgupta-dev/heraapi) wherever you use it is
appreciated.
