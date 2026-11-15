---
title: Your own code
section: Guides
order: 160
summary: Adding your own routes next to the generated ones, using the database in them, and returning errors the Cadmus way.
---

The admin and the API cover a lot, but most apps need some pages or endpoints of their own.
They go in `src/routes.rs`, as ordinary [axum](https://docs.rs/axum) routes, served next to
the generated ones.

## A route

```rust
use axum::Router;
use axum::routing::get;

pub fn router() -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}
```

## Using the database

Pass the connection to your router. In `src/main.rs`:

```rust
let app = App::from_settings(settings::settings()?).await?.migrations(migrations::all());
let db = app.db().clone();
models::register(app).routes(routes::router(db)).run().await
```

and in `src/routes.rs`:

```rust
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use cadmus::{AdminError, ApiResult};
use sea_orm::{DatabaseConnection, EntityTrait};

use crate::models::post;

pub fn router(db: DatabaseConnection) -> Router {
    Router::new()
        .route("/posts/{id}/title", get(title))
        .with_state(db)
}

async fn title(State(db): State<DatabaseConnection>, Path(id): Path<i32>) -> ApiResult<Json<String>> {
    let post = post::Entity::find_by_id(id)
        .one(&db)
        .await?
        .ok_or_else(|| AdminError::NotFound("post".into()))?;
    Ok(Json(post.title))
}
```

## Who is calling

Your routes are **public**: nothing checks a login unless you ask for one. Take
`CurrentUser` in a handler and it answers `401` to anyone who is not logged in, and tells
you who sent the rest. Take `Option<CurrentUser>` for a route everyone may use but accounts
see more of.

```rust
use axum::routing::{get, post};
use axum::{Json, Router};
use cadmus::{Action, ApiResult, CurrentUser};

pub fn router() -> Router {
    Router::new()
        .route("/hello", get(|| async { "anyone can see this" }))
        .route("/me", get(me))
        .route("/posts/{id}/publish", post(publish))
}

// Logged in: any account.
async fn me(user: CurrentUser) -> String {
    format!("logged in as {}", user.username())
}

// Logged in, and allowed to change posts.
async fn publish(user: CurrentUser) -> ApiResult<&'static str> {
    user.require("post", Action::Change)?; // 403 if not
    Ok("published")
}
```

| Logged in with | How | Notes |
|---|---|---|
| A login token | `Authorization: Bearer <token>`, from `POST /api/auth/token` | |
| An API key | `Authorization: Bearer cadmus_...` or `X-API-Key` | A read-only key may only make `GET` requests and only view. |
| The admin's session cookie | sent by the browser | Only to paths under the admin (such as `/admin/reports`). Requests that change something must send `user.csrf_token()` back in an `X-CSRF-Token` header. |

`CurrentUser` has `username()`, `is_superuser()`, `access()`, `can(model, action)`,
`require(model, action)`, `require_superuser()` and `csrf_token()`. Permissions are read
again on every request, so a deactivated account is refused at once.

Being logged in is not the same as being allowed: check permissions with `require` or `can`.
Ownership (`owned_by`) is not applied for you here either: filter on `user.username()` in
your queries.

## Errors

Every failure in Cadmus is an `AdminError`, shown in the right form for where the request
went:

| Request to | An error looks like |
|---|---|
| `/api/...` | JSON: `{"error": {"code", "message", "field"}}` |
| `/admin/...` | a page in the admin's style (`error.html`, which you can replace) |
| anything else | plain text, or whatever your handler returns |

Unknown addresses, wrong methods (`405`) and crashes in a handler become proper error
responses too. Internal details (SQL, file paths, crash messages) are logged, never sent to
the visitor.

In your own handlers, return `ApiResult<T>` to get JSON errors. Database, storage and I/O
errors convert with `?`, and `AdminError::custom` makes an error with your own status and
code:

```rust
if item.left == 0 {
    return Err(AdminError::custom(StatusCode::CONFLICT, "out_of_stock", "sold out").into());
}
```

## Logging

Messages go to the terminal (standard error). `log_level` in the settings, or the
`LOG_LEVEL` variable, sets how much: `error`, `warn`, `info` (default), `debug` (also every
SQL statement) or `trace`. Each request is logged on one line with method, address, status
and duration. If your `main` installs its own `tracing` subscriber before `run()`, that one
is used instead.

## In the API documentation

Your own routes are not discovered automatically. Add them to the documentation with
`DocsConfig::extend` (see [API documentation](api-docs.md)).
