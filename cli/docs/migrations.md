---
title: Migrations
section: Guides
order: 70
summary: Keep the database's tables in step with the models - makemigrations, migrate, renames, data changes and rolling back.
---

The database's tables must match your models. *Migrations* are the steps that get them
there, saved as files, so every copy of the project (your laptop, a colleague's, the server)
goes through the same steps.

```sh
cargo run -- makemigrations   # compares the models with the migrations so far, writes the difference
cargo run -- migrate          # applies the migrations this database has not had yet
```

## What a migration looks like

`makemigrations` writes a short Rust file into `src/migrations/` and adds it to the list in
`src/migrations/mod.rs`:

```rust
use cadmus::migrations::*;

pub fn migration() -> Migration {
    Migration::new("0002_post_changes")
        .rename_column("post", "body", "content")
        .add_column("post", Column::new("status", Type::String(None)).default("draft"))
        .drop_column("post", Column::new("views", Type::Integer))
}
```

The same file runs on SQLite, PostgreSQL and MySQL: `migrate` turns each step into the right
SQL for the database it runs on. `cargo run -- sqlmigrate 0002` prints that SQL.

## How it applies them

- Each migration runs as one transaction, so a failure leaves the database as it was.
  MySQL is the exception: it saves table changes as it goes. If a migration fails halfway
  on MySQL, look at what it changed (`sqlmigrate` shows its SQL), undo or finish that by
  hand, then run `migrate` again (or `migrate --fake` to record it as applied).
- Nothing stops two `migrate` runs at the same time. Run it once per deployment, not in
  every copy of the server (see [Deploying](deploying.md#migrations)).
- Applied migrations are recorded in the `cadmus_migration` table;
  `cargo run -- showmigrations` lists which ones this database has had.
- The built-in tables (accounts, uploads) have migrations of their own, applied before yours.
- `migrate` warns when the models have changes that no migration makes yet.

## Renames

A dropped field and a new one look the same as a rename, but a rename keeps the data. So:

- mark the field `#[sea_orm(renamed_from = "body")]`, and `makemigrations` renames it; or
- run `makemigrations` in a terminal, and it asks whether a dropped and a new field (or
  table) of the same shape are a rename.

With `--no-input`, or without a terminal, nothing counts as a rename.

## New required fields

Rows already in the table need a value for a new required field. Either give the field a
default (`#[sea_orm(default_value = "draft")]`), make it optional (`Option<...>`), or type a
value when `makemigrations` asks; it is saved in the migration as `add_column_filled`.

## Changing data

A migration can be edited until it has been applied somewhere. Add data changes with SQL:

```rust
Migration::new("0003_slugs")
    .sql_reversible(
        "UPDATE post SET slug = lower(title)",
        "UPDATE post SET slug = ''",
    )
```

or with Rust code, which runs inside the migration's transaction:

```rust
use sea_orm::ConnectionTrait;

Migration::new("0003_trim").run(|db| {
    Box::pin(async move {
        db.execute_unprepared("UPDATE post SET title = trim(title)").await?;
        Ok(())
    })
})
```

`cargo run -- makemigrations --empty --name slugs` writes an empty migration to start from.

## Rolling back

```sh
cargo run -- migrate 0001     # undo every migration after 0001
cargo run -- migrate zero     # undo all of them
```

A dropped column comes back empty (`0`, `''`, ...). SQL or code without a reverse cannot be
rolled back; `--fake` records or forgets a migration without running anything, for tables
you changed by hand.

## SQLite

SQLite cannot change or drop most columns in place, so for those steps the table is rebuilt:
created anew, rows copied, the old one dropped. Links between tables are checked afterwards.

## In CI

`cargo run -- makemigrations --check` fails if a model changed without a migration, so a
forgotten migration is caught before it is merged.

## Not handled

Database enums and array columns, changing a primary key, and changing a column's type on
PostgreSQL when the values cannot be converted. Write those steps with `.sql(...)`.

## Projects without migrations

A project that does not call `.migrations(...)` keeps the older behaviour: `migrate` adds
missing tables and columns to match the models, and never drops or renames anything. To
start keeping migrations, run `makemigrations`, then add `mod migrations;` to `main.rs` and
`.migrations(migrations::all())` to the `App`, as the command explains. The tables already
exist, so `migrate` just records the first migration as applied.
