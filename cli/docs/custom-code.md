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
