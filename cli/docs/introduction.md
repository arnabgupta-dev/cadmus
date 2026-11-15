---
title: Introduction
section: Getting started
order: 10
summary: What Cadmus is, what you get from it, and how its pieces fit together.
---

Cadmus is a batteries-included web framework for Rust. You describe your data once, as
**models**, and Cadmus gives you:

- **a database table** for each model, kept up to date with migrations;
- **an admin site** where people log in and add, edit, search and delete records;
- **a REST API** (JSON) so other programs can do the same, with interactive documentation;
- **accounts and permissions**: users, groups, two-factor login, API keys for programs, and
  login with Google, GitHub and others, for the admin and for your own apps;
- **rate limits** on every request, per account, API key or address;
- **management commands** such as `migrate` and `createsuperuser`;
- **file uploads**, to a folder or to cloud storage.

It runs on [axum](https://docs.rs/axum), the web server, and
[SeaORM](https://www.sea-ql.org/SeaORM/), the database layer, and works with SQLite,
PostgreSQL and MySQL.

## Why Cadmus

- **Batteries included, built to measure.** Accounts, permissions, an admin site, an API
  with documentation, migrations and file uploads come with the framework, configured in one
  settings file. Logins through other sites, e-mail, cloud storage and database drivers are
  features that `cadmus add` switches on, so a project compiles only what it uses.
- **Little code to write.** There are no HTML templates for the admin and no handlers for
  the API: both are generated from the models. Your own code goes where the generated parts
  stop.
- **Caught early.** It is Rust, so a misspelt setting, field or model is reported when the
  project compiles, before anything runs.
- **One program to deploy.** The result is a single fast executable, with no interpreter or
  runtime to install on the server.

## How the pieces fit

```text
            your models (src/models/*.rs)
                        │
        ┌───────────────┼────────────────┬──────────────────┐
        ▼               ▼                ▼                  ▼
  database tables   admin site       REST API          API documentation
  (migrations)      /admin           /api/...          /api/docs
                        │                │
                        └──── accounts and permissions ────┘
```

Everything shares one set of rules: an account that may not change posts cannot change
them in the admin or through the API, and neither sees buttons or endpoints it cannot use.

## Two crates

| Crate | What it is |
|---|---|
| [`cadmus`](https://crates.io/crates/cadmus) | the framework your project depends on ([API reference on docs.rs](https://docs.rs/cadmus)) |
| [`cadmus-cli`](https://crates.io/crates/cadmus-cli) | the `cadmus` command, which creates projects and models, and serves these docs to AI agents |

## Status

Cadmus is at version 0.2. Everything described here works and is tested on all three
databases, but details may still change between versions. What is not there yet is listed
under [Limitations](limitations.md).

Next: [build your first project](getting-started.md).
