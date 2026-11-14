//! Migration 0001_initial, written by `cargo run -- makemigrations`.
//!
//! It can be edited until it has been applied somewhere, for instance to change data
//! with `.sql_reversible(...)` or `.run(...)`.

use heraapi::migrations::*;

pub fn migration() -> Migration {
    Migration::new("0001_initial")
        .create_table(
            Table::new("author")
                .column(Column::new("id", Type::Integer).primary_key().auto_increment())
                .column(Column::new("name", Type::String(None))),
        )
        .create_table(
            Table::new("post")
                .column(Column::new("id", Type::Integer).primary_key().auto_increment())
                .column(Column::new("title", Type::String(None)))
                .column(Column::new("body", Type::String(None)))
                .column(Column::new("views", Type::Integer))
                .column(Column::new("author_id", Type::Integer))
                .foreign_key(ForeignKey::new("fk-post-author_id", ["author_id"], "author", ["id"])),
        )
}
