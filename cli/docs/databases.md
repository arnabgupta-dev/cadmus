---
title: Databases
section: Guides
order: 180
summary: Choosing SQLite, PostgreSQL or MySQL with one address, and leaving out the drivers you do not use.
---

The address in `DATABASE_URL` (the `database.url` setting) chooses the database. Every
driver is built in, so switching is a change to `.env`, not to the code:

| Database | `DATABASE_URL` |
|---|---|
| SQLite | `sqlite://db.sqlite3?mode=rwc` (`mode=rwc` creates the file) |
| PostgreSQL | `postgres://user:password@host:5432/name` |
| MySQL and MariaDB | `mysql://user:password@host:3306/name` |

`cadmus new blog --database postgres` (or `mysql`) starts a project with such an address to
fill in. Run `cargo run -- migrate` against a new database to create its tables.

Oracle and SQL Server are not supported: SeaORM, the database layer, has no open-source
driver for them.

## The same behaviour everywhere

The admin and the API behave the same on all three: true and false are JSON booleans, dates
and times are ISO 8601 (`2026-03-04T05:06:07`), JSON fields come back as JSON, and `?q=`
search ignores case. Migrations are written once and turned into the right SQL for each.
Cadmus's own tests run on all three.

## Connections

```rust
database: DatabaseSettings {
    url: env("DATABASE_URL")?,
    max_connections: Some(20),
    min_connections: Some(2),
    acquire_timeout: Some(Duration::from_secs(5)),
},
```

## Smaller builds

A project that only uses one database can leave the other drivers out, to compile faster:

```toml
cadmus = { version = "0.1", default-features = false, features = ["postgres"] }
```

The features are `sqlite`, `postgres` and `mysql`. An address for a driver that was left out
stops start-up and names the feature to turn back on.
