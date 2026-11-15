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

## Behind PgBouncer

[PgBouncer](https://www.pgbouncer.org) lets many copies of the server share a few
PostgreSQL connections. Cadmus works behind it, with two settings on PgBouncer's side:

```ini
[pgbouncer]
pool_mode = transaction            ; or session
max_prepared_statements = 200      ; PgBouncer 1.21 or later; needed in transaction mode
ignore_startup_parameters = extra_float_digits
```

| Pool mode | Works? |
|---|---|
| `session` | yes, with no other settings (but saves fewer connections) |
| `transaction`, PgBouncer 1.21 or later, `max_prepared_statements` above 0 | yes |
| `transaction`, `max_prepared_statements = 0` (the default) or an older PgBouncer | no: requests fail with `prepared statement "sqlx_s_1" already exists`. Turning the driver's statement cache off (`statement-cache-capacity=0` in the URL) does not help. |

Without `ignore_startup_parameters`, PgBouncer refuses the connection with
`unsupported startup parameter: extra_float_digits`.

`DATABASE_URL` then points at PgBouncer. Keep `max_connections` per copy of the server
modest (PgBouncer's `max_client_conn` must cover all copies together). Migrations work
through PgBouncer in both modes, each in its own transaction. This was tested with
PgBouncer 1.25 in front of PostgreSQL 17: migrations, account creation and 200 API requests
at once over 3 server connections (`tests/pgbouncer.rs`; set `PGBOUNCER_TEST_URL` to run it).

## Drivers

Each database driver is a Cargo feature, and a project compiles only the one it uses.
`cadmus new --database postgres` sets it; `cadmus add postgres` adds one later:

```toml
cadmus = { version = "0.2", default-features = false, features = ["postgres"] }
```

The features are `sqlite` (the default), `postgres` and `mysql` (MariaDB too). An address
for a driver that is not built in stops start-up and names the feature to add.
