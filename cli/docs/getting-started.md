---
title: Getting started
section: Getting started
order: 20
summary: Install the cadmus command and build a first project, step by step.
---

This page takes you from nothing to a running site with an admin and an API, in about ten
minutes (most of it the first compile).

## What you need

[Rust](https://www.rust-lang.org/tools/install), a recent stable version installed with
`rustup`. Nothing else: the first project uses SQLite, a database kept in a single file.

## 1. Install the cadmus command

```sh
cargo install cadmus-cli
```

This installs `cadmus`, which creates projects and models. Check it with `cadmus --version`.

## 2. Create a project

```sh
cadmus new blog
cd blog
```

This writes a ready-to-run project (see [Project layout](project-layout.md)), including a
`.env` file with a fresh secret key. For PostgreSQL or MySQL instead of SQLite, use
`cadmus new blog --database postgres` (or `mysql`) and edit the address in `.env`.

### Choose what to build in

A project compiles only the parts of Cadmus it uses. The core is always there: the admin,
the REST API and its documentation, accounts and permissions, passwords, two-factor login,
API keys, refresh tokens, rate limits and file uploads to a folder. Everything else is a
Cargo feature, which you add when you need it, now or at any time later:

```sh
cadmus new blog --features google,github,smtp   # when creating the project
cadmus add google github smtp                    # or later, inside the project
```

| Feature | What it adds |
|---|---|
| `sqlite` (the default), `postgres`, `mysql` | the database driver; `mysql` also covers MariaDB ([Databases](databases.md)) |
| `google`, `microsoft`, `github`, `facebook` | logging in with that site, in the admin and in your apps ([Login methods](login-methods.md)) |
| `oidc` | logging in with any OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, ... |
| `magic-link` | login links by e-mail |
| `smtp` | sending real e-mail; without it, e-mails are printed in the server's log ([E-mail](email.md)) |
| `s3`, `azure`, `gcs` | keeping uploaded files in Amazon S3 (or MinIO, SeaweedFS, ...), Azure Blob Storage or Google Cloud Storage ([File uploads](file-uploads.md)) |

`cadmus add` does three things: it turns the features on in `Cargo.toml`, adds login methods
to `methods()` in `src/settings.rs`, and adds the keys each one needs to `.env`, for you to
fill in. A login method whose keys are still empty is left out, so the project keeps running
until you have registered it with the provider. The next `cargo run` compiles the new parts.

To remove a feature, take it out of the `features` list in `Cargo.toml` and delete its lines
from `methods()`. A method left there without its feature stops the server at startup, as
soon as its keys are set, naming the feature to add.

Not using the `cadmus` command? The features are ordinary Cargo features:

```sh
cargo add cadmus --features google,smtp                          # SQLite is on by default
cargo add cadmus --no-default-features --features postgres,s3    # another database instead
```

## 3. Describe your data

A blog post with a title, a body, a published flag and a view count:

```sh
cadmus generate model Post title:string body:text published:bool views:int
```

This writes `src/models/post.rs` and registers it in `src/models/mod.rs`. The field types
are `string`, `text`, `int`, `bigint`, `float`, `bool`, `datetime`, `date`, `json`,
`decimal` (an exact number such as a price), `uuid`, `html` (formatted text, edited with a
rich-text editor), `file` (an uploaded file), `references` (a link to another model), `many`
(links to any number of rows of another model) and `owner` (rows belong to the account that
adds them). Add `?` to make a field optional, as in `rating:float?`. See
[Models](models.md) for the details.

## 4. Create the table

```sh
cargo run -- makemigrations   # writes src/migrations/m0001_initial.rs
cargo run -- migrate          # creates the table in the database
```

The first run compiles the project, which takes a minute or two; later runs are quick.
[Migrations](migrations.md) explains what these do.

## 5. Make an account for yourself

```sh
cargo run -- createsuperuser --username admin
```

It asks for a password. A *superuser* can do everything, including managing other
accounts.

## 6. Start the server

```sh
cargo run -- serve
```

Then open:

| Address | What you see |
|---|---|
| <http://localhost:3000/admin> | the admin site: log in, then add and edit posts |
| <http://localhost:3000/api/docs> | the API documentation, where you can try every request |
| <http://localhost:3000/> | a public status page: is the service up, does the database answer |

## 7. Change something

Give posts an optional summary: in `src/models/post.rs`, add this line to the `Model`
struct:

```rust
pub summary: Option<String>,
```

Then:

```sh
cargo run -- makemigrations   # notices the new field, writes m0002_post_summary.rs
cargo run -- migrate          # adds the column; existing posts keep their data
```

That is the whole loop: change the models, make a migration, apply it.

## Where to go next

- [Models](models.md): field types and the admin options for each model.
- [The admin site](admin.md): lists, filters, actions and forms.
- [The REST API](rest-api.md): what other programs can do, and how they log in.
- [Building with AI agents](ai-agents.md): let a coding agent build on Cadmus for you.
