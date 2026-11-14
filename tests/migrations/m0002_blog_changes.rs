//! Migration 0002_blog_changes, written by `cargo run -- makemigrations`.
//!
//! It can be edited until it has been applied somewhere, for instance to change data
//! with `.sql_reversible(...)` or `.run(...)`.

use cadmus::migrations::*;

pub fn migration() -> Migration {
    Migration::new("0002_blog_changes")
        .rename_column("post", "body", "content")
        .create_table(
            Table::new("comment")
                .column(Column::new("id", Type::Integer).primary_key().auto_increment())
                .column(Column::new("post_id", Type::Integer))
                .column(Column::new("text", Type::Text))
                .foreign_key(ForeignKey::new("fk-comment-post_id", ["post_id"], "post", ["id"]).on_delete(Action::Cascade)),
        )
        .add_column("author", Column::new("email", Type::String(None)).null())
        .add_column("post", Column::new("status", Type::String(None)).default("draft"))
        .alter_column(
            "post",
            Column::new("title", Type::String(None)),
            Column::new("title", Type::String(Some(200))),
        )
        .drop_column("post", Column::new("views", Type::Integer))
        .add_index("post", Index::new("idx-post-title", ["title"]))
}
