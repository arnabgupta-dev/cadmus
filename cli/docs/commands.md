---
title: Commands
section: Reference
order: 210
summary: Every command - the cadmus tool, and the management commands each project has.
---

## The cadmus command

Installed with `cargo install cadmus-cli`.

| Command | What it does |
|---|---|
| `cadmus new NAME [--database sqlite\|postgres\|mysql] [--features F,...] [--framework-path PATH]` | create a project in a new folder, with only the features named built in |
| `cadmus add FEATURE ...` | build in more in the project in the current folder: edits `Cargo.toml`, adds login methods to `methods()` in `src/settings.rs` and their keys to `.env` |
| `cadmus generate model NAME field:type ...` | add a model to the project in the current folder (`cadmus g model` for short) |
| `cadmus delete model NAME [--yes]` | remove a model: its file and its lines in `src/models/mod.rs` (see [Deleting a model](models.md#deleting-a-model)) |
| `cadmus secret` | print a new secret key |
| `cadmus mcp` | serve the MCP protocol for AI agents (see [Building with AI agents](ai-agents.md)) |

`--framework-path` uses a local copy of Cadmus instead of the version on crates.io.

The features are: `sqlite`, `postgres`, `mysql` (databases); `s3`, `azure`, `gcs` (file
storage); `google`, `microsoft`, `github`, `facebook`, `oidc`, `magic-link` (login methods);
`smtp` (sending e-mail). Passwords, two-factor login, API keys, refresh tokens and rate
limits are always built in.

## Project commands

Every project is also its own command line. Run them as `cargo run -- <command>`, or
`./target/release/<project> <command>` for a built program.

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

Add `--help` to any command for its options.

## makemigrations

| Option | Effect |
|---|---|
| `--name N` | name the migration (default: made up from the changes) |
| `--empty` | write a migration with no changes, to fill in by hand |
| `--check` | write nothing; fail if the models have changes without a migration |
| `--no-input` | ask nothing: nothing is taken as a rename, and new required fields need a default |

## migrate

| Example | Effect |
|---|---|
| `migrate` | apply every migration this database has not had |
| `migrate 0002` | roll back the migrations after `0002` (or apply up to it) |
| `migrate zero` | roll back every migration of the project |
| `migrate 0002 --fake` | record or forget migrations without running them |
