---
title: Project layout
section: Getting started
order: 30
summary: The files cadmus new writes, and what each one is for.
---

`cadmus new blog` writes this:

```text
blog/
├── Cargo.toml              Rust's project file: name and dependencies
├── .env                    secrets for this machine: database address, secret key (not in git)
├── .env.example            the same without the secrets, to share
├── .gitignore
├── AGENTS.md               how to work on the project, for AI coding agents
└── src/
    ├── main.rs             starts the app: settings, migrations, models, your routes
    ├── settings.rs         how the site behaves: every setting in one place
    ├── models/
    │   ├── mod.rs          lists the models, with the admin options for each
    │   └── post.rs         one file per model (cadmus generate model adds them)
    ├── migrations/
    │   ├── mod.rs          the list of migrations, kept up to date by makemigrations
    │   └── m0001_initial.rs
    └── routes.rs           your own pages and endpoints, next to the generated ones
```

## main.rs

```rust
mod migrations;
mod models;
mod routes;
mod settings;

use cadmus::{AdminError, App};

#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_settings(settings::settings()?)
        .await?
        .migrations(migrations::all());
    models::register(app).routes(routes::router()).run().await
}
```

`run()` reads the command line, so the same program is the web server (`serve`) and every
[management command](commands.md) (`migrate`, `createsuperuser`, ...).

## models/mod.rs

```rust
pub mod post;
// cadmus:modules (generated modules are added above this line)

pub fn register(app: App) -> App {
    app
        .model::<post::Entity>(cadmus::ModelOptions::new().label("Posts"))
    // cadmus:models (generated models are registered above this line)
}
```

`cadmus generate model` adds a line above each marker comment, so keep the two comments.
Change the `ModelOptions` freely: see [Models](models.md).

## settings.rs

Every setting of the site, in plain Rust: database, login methods, the admin's look, e-mail,
uploads. Secrets are read from `.env`. See [Settings](settings.md).

## migrations/

One file per migration, written by `cargo run -- makemigrations`, and `mod.rs`, which lists
them in order. You can edit a migration until it has been applied somewhere. See
[Migrations](migrations.md).

## routes.rs

Your own axum routes, served next to the generated ones:

```rust
pub fn router() -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}
```

See [Your own code](custom-code.md).

## .env

```sh
DATABASE_URL=sqlite://db.sqlite3?mode=rwc
SECRET_KEY=...            # signs logins; `cadmus secret` prints a new one
DEBUG=true                # lets the login cookie work over plain http:// on your machine
PUBLIC_URL=http://localhost:3000
```

`.env` is loaded automatically on your machine and is in `.gitignore`. On a server, set the
same names as environment variables instead.
