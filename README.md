# Cadmus

[![CI](https://github.com/arnabgupta-dev/cadmus/actions/workflows/ci.yml/badge.svg)](https://github.com/arnabgupta-dev/cadmus/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cadmus.svg)](https://crates.io/crates/cadmus)
[![docs.rs](https://img.shields.io/docsrs/cadmus)](https://docs.rs/cadmus)

**[Website and documentation](https://arnabgupta-dev.github.io/cadmus/)** ·
[crates.io](https://crates.io/crates/cadmus) ·
[API reference](https://docs.rs/cadmus) ·
[Building with AI agents](https://arnabgupta-dev.github.io/cadmus/docs/ai-agents.html)

**A batteries-included web framework for Rust.** Describe your data once, as a model, and
Cadmus gives you:

- **a database table** for it, kept up to date with migrations;
- **an admin site** where people log in and add, edit, search and delete records;
- **a REST API** (JSON) so other programs can do the same, with interactive documentation;
- **accounts and permissions**: users, groups, two-factor login, login with Google and others;
- **management commands** such as `migrate` and `createsuperuser`;
- **file uploads**, to your disk or to cloud storage.

It is built on [axum](https://docs.rs/axum) (the web server) and
[SeaORM](https://www.sea-ql.org/SeaORM/) (the database layer), and works with SQLite,
PostgreSQL and MySQL.

**Status:** early (version 0.1). Everything described here works and is tested on all three
databases, but details may still change between versions.

## Contents

- [Why Cadmus](#why-cadmus)
- [Your first project, step by step](#your-first-project-step-by-step)
- [What is in a project](#what-is-in-a-project)
- [Core ideas](#core-ideas)
  - [Models](#models)
  - [The admin site](#the-admin-site)
  - [The REST API](#the-rest-api)
  - [Migrations](#migrations)
  - [Accounts and permissions](#accounts-and-permissions)
  - [People log in, programs use keys](#people-log-in-programs-use-keys)
  - [Settings](#settings)
- [Guides](#guides): [relations](#relations), [login methods](#login-methods),
  [e-mail](#e-mail), [two-factor authentication](#two-factor-authentication),
  [login attempt limits](#login-attempt-limits), [file uploads](#file-uploads),
  [API documentation](#api-documentation), [databases](#databases),
  [errors and your own code](#errors-and-your-own-code), [home page](#home-page),
  [deploying](#deploying)
- [Building with AI agents](#building-with-ai-agents)
- [Reference](#reference): [commands](#management-commands), [settings](#all-settings),
  [migrations in detail](#migrations-in-detail)
- [Not done yet](#not-done-yet) · [Working on Cadmus](#working-on-cadmus) ·
  [License](#license)

## Why Cadmus

- **Batteries included.** Accounts, permissions, an admin site, an API with documentation,
  migrations and file uploads come with the framework, configured in one settings file. There
  is nothing to choose and wire together before you can start.
- **Little code to write.** There are no HTML templates for the admin and no handlers for the
  API: both are generated from the models. Your own code goes where the generated parts stop.
- **Caught early.** It is Rust, so a misspelt setting, field or model is reported when the
  project compiles, before anything runs.
- **One program to deploy.** The result is a single fast executable, with no interpreter or
  runtime to install on the server.

## Your first project, step by step

You need [Rust](https://www.rust-lang.org/tools/install) (a recent stable version, installed
with `rustup`). Nothing else: the first project uses SQLite, a database kept in a single
file.

**1. Install the `cadmus` command**, which creates projects and models:

```sh
cargo install cadmus-cli
```

**2. Create a project:**

```sh
cadmus new blog
cd blog
```

This writes a ready-to-run project (see [What is in a project](#what-is-in-a-project)),
including a `.env` file with a fresh secret key. For PostgreSQL or MySQL instead of SQLite,
use `cadmus new blog --database postgres` (or `mysql`) and edit the address in `.env`.

**3. Describe your data.** A blog post with a title, a body, a published flag and a view
count:

```sh
cadmus generate model Post title:string body:text published:bool views:int
```

This writes `src/models/post.rs` and registers it. The field types are `string`, `text`,
`int`, `bigint`, `float`, `bool`, `datetime`, `date`, `json`, `file` (an uploaded file) and
`references` (a link to another model, see [Relations](#relations)). Add `?` to make a field
optional, as in `rating:float?`.

**4. Create the table:**

```sh
cargo run -- makemigrations   # writes src/migrations/m0001_initial.rs, describing the table
cargo run -- migrate          # creates it in the database
```

The first run compiles the project, which takes a minute or two; later runs are quick.

**5. Make an account for yourself:**

```sh
cargo run -- createsuperuser --username admin
```

It asks for a password. A *superuser* can do everything, including managing other accounts.

**6. Start the server:**

```sh
cargo run -- serve
```

Then open:

| Address | What you see |
|---|---|
| <http://localhost:3000/admin> | the admin site: log in, then add and edit posts |
| <http://localhost:3000/api/docs> | the API documentation, where you can try every request |
| <http://localhost:3000/> | a public status page (is the service up, does the database answer) |

**7. Change something.** Give posts an optional summary: in `src/models/post.rs`, add the
line `pub summary: Option<String>,` to the `Model` struct. Then:

```sh
cargo run -- makemigrations   # notices the new field, writes m0002_post_summary.rs
cargo run -- migrate          # adds the column; existing posts keep their data
```

That is the whole loop: change the models, make a migration, apply it.

## What is in a project

```text
blog/
├── Cargo.toml              Rust's project file: name and dependencies
├── .env                    secrets for this machine: database address, secret key (not in git)
├── .env.example            the same without the secrets, to share
└── src/
    ├── main.rs             starts the app: settings, migrations, models, your routes
    ├── settings.rs         how the site behaves: every setting in one place
    ├── models/
    │   ├── mod.rs          lists the models, with the admin options for each
    │   └── post.rs         one file per model
    ├── migrations/
    │   ├── mod.rs          the list of migrations, kept by makemigrations
    │   └── m0001_initial.rs
    └── routes.rs           your own pages and endpoints, next to the generated ones
```

`main.rs` is short:

```rust
#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_settings(settings::settings()?)
        .await?
        .migrations(migrations::all());
    models::register(app).routes(routes::router()).run().await
}
```

`run()` reads the command line, so the same program is the server (`serve`) and every
[management command](#management-commands).

## Core ideas

### Models

A model describes one kind of record: its fields and their types. Cadmus uses SeaORM
entities as models; `cadmus generate model` writes one for you:

```rust
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "post")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub title: String,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    pub published: bool,
    pub views: i32,
    pub summary: Option<String>,   // Option = may be empty
}
```

Registering a model, in `src/models/mod.rs`, is one line, plus any options for the admin:

```rust
app.model::<post::Entity>(
    ModelOptions::new()
        .label("Posts")                                   // name in the menu
        .list_display(["id", "title", "published"])       // columns on the list page
        .search_fields(["title"])                         // what the search box looks in
        .list_filter(["published", "created_at"])         // filter dropdowns above the list
        .readonly_fields(["views"])                       // shown, but not editable
        .hidden_fields(["internal_note"])                 // never shown or sent anywhere
        .set_action("publish", "Mark as published", "published", "true"),
)
```

Every registered model gets its admin pages and its API endpoints at once.

### The admin site

`/admin` is where people manage the data in a browser. It works on phones as well as
desktops, has light and dark themes, and shows each account only what it may do.

- **Dashboard:** the models the account can see, with how many records each has and what the
  account may do with them.
- **Lists:** search, sorting by clicking a column heading, and pages of results. On phones
  each row becomes a card.
- **Filters** (`list_filter`): a dropdown per column above the list: Yes/No for
  true/false fields, names for links to other models, periods ("Past 7 days", "This month")
  for dates, the values present for other columns, and "(empty)" for optional ones. Filters
  combine with search, and sorting and paging keep them.
- **Acting on several rows:** tick rows (or all on the page), then pick an action. "Delete
  selected" asks first, and keeps rows that other rows still point to.
  `set_action(name, label, column, value)` adds an action that sets a column on every
  selected row; `action(name, label, |ids| async move { ... })` runs your own code with the
  selected ids. Both need the change permission; at most 1000 rows at a time.
- **Forms:** buttons appear only for what the account may do, a record it cannot change opens
  read-only, and deleting asks for confirmation. Links to other models are dropdowns, and file
  fields have a file picker with a preview for images.
- **Keyboard:** <kbd>/</kbd> jumps to the search box, and every control can be reached with the
  keyboard.
- **Your own look:** site title, colour, logo, theme and more are [settings](#settings). The
  pages load the Fira Sans typeface from Google Fonts; `admin.web_fonts: false` uses the
  system's typeface instead and contacts no other site.

### The REST API

Everything in the admin can also be done by a program, over HTTP with JSON. For every model,
under `/api`:

| Request | What it does |
|---|---|
| `GET /api/post` | list posts: `{"data": [...], "meta": {"total", "page", "per_page"}}` |
| `GET /api/post?q=rust&sort=-id&page=2&per_page=50` | search, sort (`-` for highest first), page (at most 100 per page) |
| `GET /api/post?published=true&views__gte=100` | filter (see below) |
| `POST /api/post` | create one; answers `201` with the new record |
| `GET /api/post/1` | read one |
| `PUT /api/post/1` | replace one: every required field must be sent |
| `PATCH /api/post/1` | change only the fields sent |
| `DELETE /api/post/1` | delete one; answers `204` |

**Filtering** works on any visible column: `?published=true&author_id=3`; comparisons with
`__gt`, `__gte`, `__lt` and `__lte` (`?views__gte=100`, `?created_at__gte=2026-01-01`);
`__isnull=true` or `false`; and for dates `__range=today`, `7d`, `30d`, `month` or `year`
(UTC). An unknown column, or a value the column cannot hold, is refused with `422`. A column
named `page`, `per_page`, `q` or `sort` cannot be filtered on.

**Errors** come back as JSON, `{"error": {"code", "message", "field"}}`, with a status such
as `401` (not logged in), `403` (not allowed), `404` (not found), `409` (conflicts with other
data, e.g. a duplicate), `422` (invalid field) or `429` (too many attempts). Fields that are
unknown, hidden or read-only are refused rather than silently ignored.

Every request needs a login token or an API key; see
[People log in, programs use keys](#people-log-in-programs-use-keys). The API follows the
same permissions as the admin. Its interactive documentation is at `/api/docs`
([more](#api-documentation)).

### Migrations

The database's tables must match your models. *Migrations* are the steps that get them there,
saved as files so every copy of the project (your laptop, a colleague's, the server) goes
through the same steps:

```sh
cargo run -- makemigrations   # compares the models with the migrations so far, writes the difference
cargo run -- migrate          # applies the migrations this database has not had yet
```

A migration is a short Rust file you can read and edit:

```rust
pub fn migration() -> Migration {
    Migration::new("0002_post_changes")
        .rename_column("post", "body", "content")
        .add_column("post", Column::new("status", Type::String(None)).default("draft"))
        .drop_column("post", Column::new("views", Type::Integer))
}
```

- **One file, every database:** the same migration runs on SQLite, PostgreSQL and MySQL;
  `migrate` turns it into the right SQL for each.
- **Safe:** each migration runs as one transaction, so a failure leaves the database as it
  was (MySQL is the exception: it saves table changes as it goes).
- **History:** applied migrations are recorded in the `cadmus_migration` table, and
  `showmigrations` lists them.
- **Undo:** `cargo run -- migrate 0001` rolls back everything after `0001`.
- **Renames:** `makemigrations` asks whether a column was renamed or replaced, so data is not
  lost by accident. Mark the field `#[sea_orm(renamed_from = "body")]` to say so up front.

The details, including data changes and rollbacks, are under
[Migrations in detail](#migrations-in-detail).

### Accounts and permissions

Each account has three switches:

| Switch | Meaning |
|---|---|
| Active | Can log in. Untick to lock someone out without deleting the account. |
| Staff | Can use the admin and the API. |
| Superuser | Can do everything, including managing accounts. |

Staff who are not superusers can only do what they were given: for each model, any of
**view**, **add**, **change** and **delete**. Permissions are given to an account directly or
to a **group** of accounts (such as "Editors"); an account has both. Whoever may change a
model may also view it, and file uploads count as a model named `files`.

Superusers manage accounts and groups in the admin, under **Users** and **Groups**. The same
rules apply in the admin and the API, and the admin hides links and buttons an account cannot
use.

- Changes apply at once: permissions are checked on every request, so deactivating an account
  locks it out immediately, even if it is logged in.
- Nobody can remove their own access or delete their own account, so a superuser always
  remains. If you are locked out anyway, `cargo run -- promote --username U` makes an
  account superuser again.
- Accounts also have an e-mail address (unique; it can be used to log in), first and last
  name and phone, and record when they joined and last logged in. For fields of your own, add
  a model linked to `cadmus::user::Entity` (one profile row per account).
- Passwords are stored as Argon2id hashes, never in plain text.

### People log in, programs use keys

There are two kinds of visitors, and each gets its own way in.

**People** log in with a username (or e-mail address) and password, or another
[login method](#login-methods) such as Google, plus their two-factor code if they turned it
on. In the admin this happens on the login page. An app that people use, such as a mobile
app, does the same through the API:

```sh
curl -X POST http://localhost:3000/api/auth/token \
     -H "Content-Type: application/json" \
     -d '{"username": "admin", "password": "...", "totp": "123456"}'
# → {"access_token": "eyJ...", ...}

curl -H "Authorization: Bearer eyJ..." http://localhost:3000/api/post
```

The token (a JWT) lasts an hour (`auth.token_lifetime`), after which the person logs in
again.

**Programs** running on their own have nobody to type a password or a phone code: a nightly
import script, another company's server, a website's backend fetching posts to show. They
use an **API key** instead: a long random string, made once and pasted into the program's
configuration.

```sh
curl -H "Authorization: Bearer cadmus_..." https://example.com/api/post
curl -H "X-API-Key: cadmus_..." https://example.com/api/post     # the same
```

| | Login token | API key |
|---|---|---|
| For | a person, in an app | a program running on its own |
| Made by | logging in with password (and two-factor code) | once, on the account's API keys page |
| Lasts | an hour | until revoked, or 30 days / 90 days / a year / never |
| If it leaks | change the password | revoke that one key; nothing else changes |
| Can be limited | no | read-only |

How keys work:

- **Making one:** each account has an **API keys** page (the key icon under its name in the
  admin's menu), or run `cargo run -- apikey --username U --name "import script"`. The key is
  shown once; only a fingerprint (SHA-256 hash) of it is stored.
- **What it can do:** act as its account, with the account's permissions, through the API
  only (never the admin pages). A **read-only** key can view but not add, change or delete.
- **When it stops:** when revoked, when it expires, or when the account is deactivated or
  deleted. The page shows when each key was last used.
- **Safety:** making a key needs a login in the last 15 minutes or the password, so someone
  who steals a logged-in browser session cannot quietly make one. Superusers see every
  account's keys on its user page and can revoke them.
- **Not needed?** If only people use your API, leave keys unused, or turn them off with
  `auth.api_keys: false`.

The admin itself keeps the login in an `HttpOnly`, `SameSite=Strict` cookie, and every form
carries a CSRF token. The API never accepts that cookie, only a token or a key.

### Settings

A project is configured in one file, `src/settings.rs`. It is
plain Rust, so the compiler catches a misspelt setting, and your editor completes the names
and shows what each one does. Anything left out keeps its default:

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
            totp_required: true,       // everyone must use two-factor login
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

- **Secrets stay out of the code.** The database address, the secret key and passwords are
  read with `env("NAME")?`: from `.env` on your machine (loaded automatically), from the
  server's environment variables in production. `env_or("NAME", default)?` reads an optional
  value of any type (`true/false`, `1/0`, `yes/no` for yes-or-no), and `env_opt("NAME")` an
  optional text. A missing or malformed variable stops start-up and names it.
- **Checked at start-up:** a mistake stops the server with the setting it is about, e.g.
  ``admin.accent_color: accent colour must be #rgb or #rrggbb, got `red` ``.
  `cargo run -- check` runs the same checks without starting the server.
- Values that differ per machine are ordinary code: `if debug { ... }`, or
  `bind: env_or("APP_BIND", "127.0.0.1:3000".to_owned())?`.

Every setting is listed under [All settings](#all-settings).

## Guides

### Relations

A link from one model to another (a foreign key) is a `references` field. Generate the model
being pointed to first:

```sh
cadmus generate model Author name:string email:string
cadmus generate model Post title:string author:references editor:references=author?
```

`author:references` adds an `author_id` column pointing at `author`.
`editor:references=author?` adds an optional `editor_id` that also points at `author`. A model
can point to itself (`parent:references=category?` on `Category`). Migrations create the
database constraint that keeps the links valid.

In the admin, such a field is a dropdown of the related records on forms, and lists show the
related record's name, linked to it, instead of its number.

- **Which name:** a record is named by its first text field (`name`, `title`, ...). Choose
  another with `ModelOptions::new().display_field("email")`.
- **Long lists:** above 200 related records there is no dropdown; the form takes the id and
  says which record it currently points to.
- **Permissions:** names are shown only to accounts that may view the related model; others
  see and edit the plain number.
- **Deleting:** a record that others still point to cannot be deleted (`409`).
- **Hand-written entities:** any `belongs_to` relation in an entity is picked up the same way.

The API reads and writes the id itself (`author_id`).

### Login methods

Each project chooses how people log in. The login page shows the chosen methods, in order:

```rust
// Password, or Facebook
auth: AuthSettings {
    methods: vec![
        AuthMethod::Password,
        AuthMethod::Facebook(OAuthClient::from_env("FACEBOOK")?),
    ],
    ..Default::default()
},

// Google only, and anyone at acme.com gets an account on first login
auth: AuthSettings {
    methods: vec![AuthMethod::Google(OAuthClient::from_env("GOOGLE")?)],
    signup: Signup::Domains(vec!["acme.com".into()]),
    signup_groups: vec!["Staff".into()],
    ..Default::default()
},
```

| Method | What it needs |
|---|---|
| `AuthMethod::Password` | nothing; leave it out to turn passwords off everywhere, the API's token endpoint included |
| `AuthMethod::MagicLink` | [e-mail](#e-mail); people type their address and get a login link |
| `AuthMethod::Google(client)` | an OAuth client from the Google Cloud console |
| `AuthMethod::Microsoft { client, tenant }` | an app registration in Microsoft Entra; `tenant` is `common`, `organizations`, `consumers` or your directory's ID |
| `AuthMethod::GitHub(client)` | an OAuth app in GitHub's developer settings |
| `AuthMethod::Facebook(client)` | an app at developers.facebook.com with Facebook Login |
| `AuthMethod::Oidc(OidcProvider::new(name, label, issuer, client))` | any OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, ... |

`OAuthClient::from_env("GOOGLE")` reads `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` from
`.env`. Set `server.public_url` to the site's address (`PUBLIC_URL` in a generated project)
and give each provider this *redirect URI*: `<public_url>/admin/auth/<name>/callback`, e.g.
`https://shop.example.com/admin/auth/google/callback`.

**Who gets in.** A login through another site is matched to the account with the e-mail
address that site vouches for, and the two are then linked, so later logins work even if the
address changes. Addresses the site has not verified are never used. Nobody else gets in
unless `signup` allows it: `Signup::Domains(vec!["acme.com".into()])` or `Signup::Open`
create an account at the first login, as staff with no permissions of their own, in the
`signup_groups`. Each account's page lists its links, and a superuser can remove one.

- Accounts with two-factor login enter their code after coming back from the other site, and
  `totp_required` applies as for passwords. Inactive accounts cannot log in.
- To start a project without passwords, run
  `createsuperuser --username you --email you@example.com`: it asks for no password, and the
  account logs in through the provider.
- Microsoft's shared directories (`common`, `organizations`, `consumers`) let people choose any
  address, so their addresses only count if the app sends the optional `xms_edov` claim. With
  your own directory's ID as `tenant`, its addresses are trusted.
- GitHub uses the account's primary verified address. For an OpenID Connect provider that does
  not say whether addresses are verified, `trust_email: true` trusts them; only do that when
  users cannot pick their own address.
- These logins are for the admin. Programs use an [API key](#people-log-in-programs-use-keys).

### E-mail

Login links, password resets and invitations are sent by e-mail. Until `mail.url` is set,
messages are written to the server's log instead, so on your own machine you copy the link
from the terminal:

```rust
mail: MailSettings {
    url: env_opt("MAIL_URL"),   // smtps://user:password@smtp.example.com
    from: Some("Acme <noreply@acme.com>".into()),
},
```

`smtps://` (TLS, port 465) and `smtp://...?tls=required` (STARTTLS, port 587) work with any
provider's SMTP service. Unencrypted `smtp://` is accepted only to this machine, e.g. a test
inbox such as Mailpit on `smtp://localhost:1025`. To send through an HTTP API instead,
implement `cadmus::Mailer` (one `send` method) and pass it to `App::mailer`.

- **Login links** (`AuthMethod::MagicLink`): the login page asks for an address and mails a
  link that works once, within 15 minutes. Opening it shows a button rather than logging in
  straight away, because mail scanners open links to check them.
- **Forgot your password?** appears under the password field when passwords are on and
  `server.public_url` is set. The link works for an hour.
- **Changing your own password:** the lock button next to your name in the admin.
- **Invitations:** on the new-account form, "Send an invitation instead" e-mails a link, valid
  once for 7 days, where the person chooses their password. The account's page can send it
  again.
- The answer to "send me a link" is the same whether or not the address has an account, and
  mail is sent in the background, so nobody can find out who is registered. Requests are
  limited like login attempts.

### Two-factor authentication

Two-factor login asks for a code from an app on your phone (Google Authenticator, 1Password,
Aegis, ...) as well as the password, so a stolen password alone is not enough.

Each person turns it on for themselves: click your name in the admin menu, scan the QR code,
and confirm with a code. From then on logging in needs the current code: in the login form, or
as `"totp"` in an API token request.

- Each code works once, so an intercepted code cannot be reused.
- **Recovery codes:** turning it on shows ten codes, once. Each logs in a single time in place
  of a phone code, for when the phone is lost. The two-factor page shows how many are left and
  makes a new set. Only a fingerprint of each code is stored.
- Turning it off asks for a current code (or a recovery code). Turning it on needs the
  password too, if the login is more than 15 minutes old.
- **Lost phone, no recovery codes:** a superuser ticks "Turn it off" on the person's account
  page, or run `cargo run -- totp --username U --disable`.
- **Making it compulsory:** `auth.totp_required: true`. Nobody can then log in without it, so
  set up the first account from the terminal: `cargo run -- totp --username U` prints the QR
  code and the recovery codes.

### Login attempt limits

To stop password guessing: after 5 failed attempts on one account within 5 minutes, further
attempts on that account are refused (`429 Too Many Requests`, with a `Retry-After` header)
until the 5 minutes are over. A successful login clears the count. One client address gets
four times as many attempts across all accounts. Change the numbers with
`auth.login_max_attempts` (`0` turns the limit off) and `auth.login_lockout`.

- **Behind a reverse proxy** (nginx, a load balancer), every request seems to come from the
  proxy. Set `server.client_ip_header` to the header the proxy sets (e.g. `X-Forwarded-For`),
  but only if it always sets it; otherwise clients could pretend to be anyone.
- Counts are kept in memory, per running copy of the server: several copies each count
  separately, and a restart clears them.
- Someone who knows a username can keep that account locked by guessing. That is the usual
  trade-off of per-account limits; shorten `auth.login_lockout` if it becomes a problem.

### File uploads

Set `uploads.storage_url` to turn uploads on. The address chooses where files are kept:

| Address | Where | Cargo feature |
|---|---|---|
| `file:///absolute/path` | a folder on the server | built in |
| `s3://bucket/prefix` | Amazon S3 and compatible services | `s3` |
| `az://container/prefix` | Azure Blob Storage | `azure` |
| `gs://bucket/prefix` | Google Cloud Storage | `gcs` |

Cloud credentials are read from the usual environment variables (`AWS_ACCESS_KEY_ID`,
`AWS_REGION`, `AWS_ENDPOINT`, `AZURE_STORAGE_ACCOUNT_NAME`, ...).

**On admin forms:** a field generated as `photo:file` (or an integer column listed in
`ModelOptions::file_fields(["photo"])`) holds a file. Its form field is a file picker that
shows the current file, a preview for PNG, JPEG, GIF, WebP and AVIF images, and "Remove the
file" when the field is optional. Lists show the file's name, linked to it. Anyone who may view
the record may download its file. Files from a form that is then rejected are removed.

**Through the API:**

| Request | What it does |
|---|---|
| `POST /api/files` (multipart form, field `file`) | uploads a file; answers `201` with its record, whose `id` goes in file fields |
| `GET /api/files`, `GET /api/files/{id}` | file details |
| `GET /api/files/{id}/content` | downloads the file |
| `DELETE /api/files/{id}` | deletes it |

**Large files** can be sent in pieces, and the upload resumes after a dropped connection:

1. `POST /api/uploads` with `{"filename", "size", "content_type"}` gives an upload `id`.
2. `PATCH /api/uploads/{id}` with the header `Upload-Offset: N` and the next bytes, in order. A
   wrong offset gets `409` with the right one.
3. `GET /api/uploads/{id}` tells how much has arrived: that is where a client resumes.
4. The piece that completes the file answers `201` with the file's record.
5. `DELETE /api/uploads/{id}` cancels.

Uploads survive server restarts and can continue on another server. Stored names are chosen by
the server; the original file name is kept only as a label. Files sent through an admin form
must finish within `server.request_timeout` (30 seconds by default), so very large files
should go through the API. Replacing or removing a file on a form leaves the old one stored
(it is still listed at `/api/files`).

In your own code, `Storage` (`put`, `get`, `delete`, `sink`) and `Uploads` are available.

### API documentation

Every project documents its own API:

| Address | What it is |
|---|---|
| `/api/docs` | Swagger UI: browse the endpoints and try them out |
| `/api/redoc` | ReDoc: the same, as a reference page |
| `/api/openapi.json` | the OpenAPI 3.0 description, for generating client code |

It is generated from the models: every endpoint, the fields returned, the fields that may be
sent and which are required, the permission each needs, and file uploads when enabled. Hidden
fields never appear, and read-only fields appear only in what is returned.

To try requests in Swagger UI, press **Authorize** and log in with a username and password, or
paste an API key under `apiKey`.

**Protecting it.** The documentation is public by default; it shows model and field names,
never data. To ask for a username and password first (the browser's own prompt), set both,
with the password in `.env`:

```rust
docs: DocsSettings {
    username: Some("docs".into()),
    password: Some(env("DOCS_PASSWORD")?),
    ..Default::default()
},
```

These only open the documentation; they are not an account. `enabled: false` removes the
documentation entirely.

- `cargo run -- openapi > openapi.json` writes the description without starting the server.
- Your own routes are not discovered; add them with
  `app.docs(DocsConfig::new().extend(|doc| { doc["paths"]["/health"] = json!({...}); }))`.
- `docs.title`, `docs.version` and `docs.description` (Markdown) set the heading.
- The viewers load from a CDN (jsDelivr, pinned versions), so the two pages need internet
  access in the browser. To host them yourself, set `docs.swagger_js_url`,
  `docs.swagger_css_url` and `docs.redoc_js_url`.

### Databases

The address in `database.url` chooses the database. Every
driver is built in, so switching is a change to `.env`, not to the code:

| Database | `DATABASE_URL` |
|---|---|
| SQLite | `sqlite://db.sqlite3?mode=rwc` (`mode=rwc` creates the file) |
| PostgreSQL | `postgres://user:password@host:5432/name` |
| MySQL and MariaDB | `mysql://user:password@host:3306/name` |

Oracle and SQL Server are not supported: SeaORM, the database layer, has no open-source driver
for them.

The API behaves the same on all three: true and false are JSON booleans, dates and times are
ISO 8601 (`2026-03-04T05:06:07`), JSON fields come back as JSON, and `?q=` search ignores
case.

**Smaller builds:** a project that only uses one database can leave the other drivers out, to
compile faster: `cadmus = { ..., default-features = false, features = ["postgres"] }` (the
features are `sqlite`, `postgres` and `mysql`).

### Errors and your own code

`src/routes.rs` holds your own axum routes, served next to the generated ones. Every failure
in Cadmus is an `AdminError`, shown in the right form for where the request went:

| Request to | An error looks like |
|---|---|
| `/api/...` | JSON: `{"error": {"code", "message", "field"}}` |
| `/admin/...` | a page in the admin's style (`error.html`, which you can override) |
| anything else | plain text, or whatever your handler returns |

Unknown addresses, wrong methods (`405`) and crashes in a handler become proper error responses
too. Internal details (SQL, file paths, crash messages) are logged, never sent to the visitor.

In your own handlers, return `ApiResult<T>` to get JSON errors. Database, storage and I/O
errors convert with `?`, and `AdminError::custom` makes an error with your own status and code:

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

Log messages go to the terminal (standard error); `log_level` in the settings, or the
`LOG_LEVEL` variable, sets how much. `debug` also shows every SQL statement.

### Home page

`/` is a public status page: the service's name, whether it is up, whether the database
answers and how fast, and how long it has been running. It refreshes every 30 seconds and
links nowhere, so it does not lead visitors to the admin.

- **For monitoring:** the same address with `Accept: application/json` returns
  `{"status", "database": {"status", "latency_ms"}, "uptime_secs", "checked_at"}`. Both answer
  `503` when the database is down, so a load balancer can use it as a health check.
- API addresses typed into a browser's address bar are sent to `/`. Programs, scripts and tools
  like `curl` reach the API as usual, and the documentation pages are not affected.
- **Your own page:** a `/` route in `src/routes.rs` replaces it automatically. To turn it off,
  set `home.enabled: false` (do that too if your routes set their own `Router::fallback`).
  `home.title` and `home.tagline` set what it says.

### Deploying

A generated project is ready to run behind HTTPS as it is:

- **Time limit:** a request that takes longer than 30 seconds gets `503` and is stopped
  (`server.request_timeout`; `None` for no limit). Uploads and downloads are exempt.
- **Request log:** one line per request with method, address, status and duration (the query
  string is left out).
- **Protective headers** on every admin and API response: no embedding in other sites, no
  content-type guessing, no caching of pages that needed a login.
- **Cross-origin access (CORS)** is off. To let a browser app on another domain call the API,
  set `server.cors_origins: vec!["https://app.example.com".into()]` (or `"*"`). Such apps use
  a token or key; the login cookie is never accepted from another domain.
- **Graceful shutdown** on Ctrl-C.

Before going live:

1. Serve over HTTPS, and leave `DEBUG` unset in production: the generated settings only allow
   the login cookie over plain HTTP when `DEBUG=true`.
2. Behind a reverse proxy, set `server.client_ip_header` (see
   [Login attempt limits](#login-attempt-limits)), and `server.bind` to `0.0.0.0:3000` if the
   proxy runs on another machine or container.
3. Set `DATABASE_URL` and `SECRET_KEY` on the server, keep `.env` out of git, and use a
   different `SECRET_KEY` for each environment (`cadmus secret` prints a new one).
4. Run `cargo run --release -- migrate` as part of each deployment, before starting the new
   version.

Responses are not compressed, on purpose: admin pages contain a security token next to text
the visitor controls (the search box), which is what the BREACH attack on compressed HTTPS
needs.

## Building with AI agents

Cadmus offers two [MCP](https://modelcontextprotocol.io) servers, so AI assistants can work
with it:

- **Hosted docs server**, for any app that adds an MCP server by its address (ChatGPT,
  Claude, Gemini CLI, Cursor, VS Code). Nothing to install; the assistant can search and read
  the documentation. Add `https://cadmus-mcp.cadmus-mcp.workers.dev/mcp`.
- **Local server**, `cadmus mcp`, for coding agents on your computer. The agent can also
  create projects, add models and run migrations for you:

  ```sh
  claude mcp add cadmus -- cadmus mcp                 # Claude Code
  ```

  or, for clients with an `mcpServers` list (Claude Desktop, Cursor):

  ```json
  { "mcpServers": { "cadmus": { "command": "cadmus", "args": ["mcp"] } } }
  ```

Every project made by `cadmus new` also has an `AGENTS.md` guide, and the website offers
[llms.txt](https://arnabgupta-dev.github.io/cadmus/llms.txt). Details:
[Building with AI agents](https://arnabgupta-dev.github.io/cadmus/docs/ai-agents.html).

## Reference

### Management commands

Run as `cargo run -- <command>` (or `./target/release/<project> <command>` for a built
program).

| Command | What it does |
|---|---|
| `serve [--bind addr]` | start the server |
| `makemigrations [--name N] [--empty] [--check] [--no-input]` | write a migration for the model changes into `src/migrations` |
| `migrate [target] [--fake]` | apply new migrations, or roll back to `target` (a name, a number such as `0002`, or `zero`) |
| `showmigrations` | list migrations and whether this database has had each |
| `sqlmigrate NAME [--backwards]` | print the SQL a migration runs on this database |
| `createsuperuser --username U [--email E] [--password-stdin]` | create a superuser |
| `changepassword --username U [--password-stdin]` | change a password |
| `promote --username U` | make an existing account an active superuser |
| `totp --username U [--disable]` | turn two-factor login on (prints a QR code) or off |
| `apikey --username U --name N [--days D] [--read-only]` | make an API key (90 days by default; `--days 0` for never) |
| `openapi` | print the API description as JSON |
| `routes` | list the generated addresses |
| `check` | check the configuration without starting the server |

And the `cadmus` command:

| Command | What it does |
|---|---|
| `cadmus new NAME [--database sqlite\|postgres\|mysql] [--framework-path PATH]` | create a project |
| `cadmus generate model NAME field:type ...` | add a model to the project in the current folder |
| `cadmus delete model NAME [--yes]` | remove a model: its file and its lines in `src/models/mod.rs` |
| `cadmus secret` | print a new secret key |
| `cadmus mcp` | serve the MCP protocol for AI agents |

### All settings

| Section | Settings (defaults in brackets) |
|---|---|
| `secret_key` | signs login tokens; at least 32 bytes (`cadmus secret` prints one) |
| `database` | `url`, `max_connections`, `min_connections`, `acquire_timeout` |
| `server` | `bind` (`127.0.0.1:3000`), `request_timeout` (30 s; `None` for no limit), `cors_origins`, `client_ip_header`, `public_url` |
| `auth` | `methods` (password), `signup`, `signup_groups`, `token_lifetime` (1 h), `totp_required`, `cookie_secure` (`true`), `login_max_attempts` (5), `login_lockout` (5 min), `api_keys` (`true`) |
| `admin` | `path` (`/admin`), `site_title`, `accent_color`, `logo_url`, `theme`, `custom_css_url`, `footer_text`, `page_size` (25), `nav_links`, `web_fonts`, `template_dir` |
| `api` | `path` (`/api`) |
| `docs` | `enabled`, `username` + `password`, `title`, `version`, `description`, `swagger_js_url`, `swagger_css_url`, `redoc_js_url` |
| `home` | `enabled`, `title`, `tagline` |
| `uploads` | `storage_url` (off until set), `max_file_bytes` (1 GiB), `max_chunk_bytes` (8 MiB) |
| `mail` | `url` (SMTP; unset writes messages to the log), `from` |
| `log_level` | `LogLevel::INFO`; `DEBUG` adds SQL statements |

Durations are written `Duration::from_secs(600)`. Things a setting cannot hold (your own
routes in the API documentation, your own account store) are set on the `App` after it is
built, e.g. `App::from_settings(settings()?).await?.docs(DocsConfig::new().extend(...))`.

### Migrations in detail

- **Renames:** a field marked `#[sea_orm(renamed_from = "body")]` is renamed. Otherwise, in a
  terminal, `makemigrations` asks whether a dropped and a new column (or table) of the same
  shape are a rename. With `--no-input`, or without a terminal, nothing counts as a rename.
- **New required fields:** rows already in the table need a value. Give the field a default
  (`#[sea_orm(default_value = "draft")]`), or type a value when asked; it is saved in the
  migration as `add_column_filled`.
- **Editing a migration:** allowed until it has been applied somewhere. Add data changes with
  `.sql_reversible("UPDATE ...", "UPDATE ...")` or with Rust code,
  `.run(|db| Box::pin(async move { ... }))`. `makemigrations --empty` writes an empty one to
  start from.
- **Rolling back:** `migrate 0001` undoes the migrations after `0001`; `migrate zero` undoes
  all. A dropped column comes back empty (`0`, `''`, ...). SQL or code without a reverse cannot
  be rolled back; `--fake` forgets such a migration without running anything.
- **SQLite** cannot change or drop most columns in place, so for those steps the table is
  rebuilt: created anew, rows copied, the old one dropped. Links between tables are checked
  afterwards.
- **In CI:** `makemigrations --check` fails if a model changed without a migration, and
  `migrate` warns about such changes.
- **Not handled:** database enums and array columns, changing a primary key, and changing a
  column's type on PostgreSQL when the values cannot be converted (write that step with
  `.sql(...)`).
- **Built-in tables** (accounts, uploads) have migrations of their own, applied before yours.

**Projects made before migrations existed:** run `makemigrations`, then add `mod migrations;`
to `main.rs` and `.migrations(migrations::all())` to the `App`, as the command explains. The
tables already exist, so `migrate` just records the first migration as applied. Until then,
`migrate` keeps the old behaviour: it adds missing tables and columns, and never drops or
renames anything. Databases from before migrations get their built-in tables brought up to
date the first time `migrate` runs.

<details>
<summary>Projects configured from environment variables only (<code>App::from_env</code>)</summary>

Projects made before `src/settings.rs` existed start with `App::from_env()`, which reads
everything from environment variables. It still works. To move to settings, run `cadmus new`
for a fresh `src/settings.rs`, copy it in, replace `App::from_env()` with
`App::from_settings(settings::settings()?)`, and rename `ADMIN_JWT_SECRET` to `SECRET_KEY`
in `.env`.

| Variable | Meaning | Default |
|---|---|---|
| `DATABASE_URL` | **Required** | — |
| `ADMIN_JWT_SECRET` | **Required.** Signing secret, at least 32 bytes | — |
| `APP_BIND` | Listen address | `127.0.0.1:3000` |
| `REQUEST_TIMEOUT_SECS` | Longest a request may take; `0` for no limit | `30` |
| `CORS_ALLOWED_ORIGINS` | Origins allowed to call the app from a browser | none |
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug` (adds SQL statements) or `trace` | `info` |
| `ADMIN_BASE_PATH`, `API_BASE_PATH` | Address prefixes | `/admin`, `/api` |
| `DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS`, `DB_ACQUIRE_TIMEOUT_SECS` | Connection pool | driver defaults |
| `ADMIN_JWT_TTL_SECS` | Token lifetime | `3600` |
| `ADMIN_TOTP_REQUIRED` | Refuse accounts without two-factor login | `false` |
| `ADMIN_LOGIN_MAX_ATTEMPTS` | Login attempts per account per lockout period; `0` disables | `5` |
| `ADMIN_LOGIN_LOCKOUT_SECS` | Length of the lockout period | `300` |
| `ADMIN_CLIENT_IP_HEADER` | Header carrying the client address behind a proxy | connection address |
| `ADMIN_COOKIE_SECURE` | `false` only for local plain HTTP | `true` |
| `STORAGE_URL` | Enables uploads | off |
| `DOCS_ENABLED` | Serve the API documentation | `true` |
| `DOCS_USERNAME`, `DOCS_PASSWORD` | Ask for these before showing the documentation | open |
| `DOCS_TITLE`, `DOCS_VERSION`, `DOCS_DESCRIPTION` | Documentation heading | site title + "API", `0.1.0` |
| `DOCS_SWAGGER_JS_URL`, `DOCS_SWAGGER_CSS_URL`, `DOCS_REDOC_JS_URL` | Where the documentation viewers load from | jsDelivr |
| `UPLOAD_MAX_FILE_BYTES`, `UPLOAD_MAX_CHUNK_BYTES` | Upload limits | 1 GiB, 8 MiB |
| `ADMIN_SITE_TITLE`, `ADMIN_ACCENT_COLOR`, `ADMIN_LOGO_URL`, `ADMIN_THEME`, `ADMIN_CUSTOM_CSS_URL`, `ADMIN_FOOTER_TEXT`, `ADMIN_PAGE_SIZE`, `ADMIN_NAV_LINKS`, `ADMIN_TEMPLATE_DIR`, `ADMIN_WEB_FONTS` | Admin look | see `UiSettings::from_env` |
| `HOME_PAGE_ENABLED` | Serve the status page at `/` | `true` |
| `HOME_TITLE`, `HOME_TAGLINE` | What the status page says | site title, none |

</details>

**Your own account store:** accounts live in the `auth_user` table (`DbUserStore`). To use
accounts kept elsewhere, implement the `UserStore` trait. Every account is then a superuser
unless you implement `UserStore::access`, API keys work only if you implement
`UserStore::api_key`, and the Users and Groups pages are not shown.

## Not done yet

- **Accounts:** no passkeys yet. API keys cannot be limited to some models (only read-only).
  Logins through other sites are for the admin, and an account is linked to another site by
  e-mail address only (there is no "connect" button). No per-record permissions. Login
  attempt limits are per server process. Accounts and groups are managed in the admin only;
  there is no API for them.
- **Admin:** no page listing all uploaded files, and a replaced file is not deleted. No
  many-to-many widget, no list of the records that point to a record, and no search-as-you-type
  for long lists of related records.
- **Models:** single-column primary keys only; UUID, decimal, enum and array fields are
  read-only.
- **Uploads:** completing a resumable upload copies the pieces once inside the storage
  (no direct-to-S3 multipart yet), and abandoned uploads are not cleaned up automatically.
- **Server:** no response compression (see [Deploying](#deploying)), request ids or metrics;
  add them with your own `tower` layers.
- **API documentation:** your own routes are not discovered automatically, and the viewers
  load from a CDN rather than being bundled.
- **Untested services:** S3, Azure and Google Cloud Storage compile but have not been run
  against the real services.

## Working on Cadmus

To try the latest unreleased version, install the command with
`cargo install --git https://github.com/arnabgupta-dev/cadmus cadmus-cli`. To use a local copy
of Cadmus in a project: `cargo install --path cli`, then
`cadmus new blog --framework-path /path/to/cadmus`.

Running the tests:

```sh
cargo test                       # SQLite, in memory

# the same tests on PostgreSQL (each test gets its own schema)
docker run -d --rm -e POSTGRES_PASSWORD=test -p 5432:5432 postgres:17-alpine
TEST_DATABASE_URL=postgres://postgres:test@localhost:5432/postgres cargo test

# or on MySQL (each test gets its own database)
docker run -d --rm -e MYSQL_ROOT_PASSWORD=test -p 3306:3306 mysql:8.4
TEST_DATABASE_URL=mysql://root:test@localhost:3306/mysql cargo test
```

## Feedback

Found a bug or have an idea? Open an issue:

- [Report a bug](https://github.com/arnabgupta-dev/cadmus/issues/new?template=bug_report.yml)
- [Suggest something](https://github.com/arnabgupta-dev/cadmus/issues/new?template=suggestion.yml)

## License

[MIT](LICENSE). Free to use and change, commercially or otherwise. Copies must keep the
copyright notice; beyond that, a link back to
[this repository](https://github.com/arnabgupta-dev/cadmus) wherever you use it is
appreciated.

The admin uses three icons from [Lucide](https://lucide.dev), under the ISC license; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
