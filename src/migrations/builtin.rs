//! Migrations for the built-in tables: accounts (`auth`) and uploads (`uploads`).
//!
//! These are frozen: when a built-in model changes, add a migration here rather than editing
//! one, because databases record the ones they have had. A test checks that each list builds
//! exactly the tables the models describe.

use super::*;

/// The account tables.
pub(crate) fn auth() -> Vec<Migration> {
    vec![
        Migration::new("0001_initial")
            .create_table(
                Table::new("auth_group")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("name", Type::String(None)).unique()),
            )
            .create_table(
                Table::new("auth_group_permission")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("group_id", Type::Integer))
                    .column(Column::new("model", Type::String(None)))
                    .column(Column::new("action", Type::String(None)))
                    .index(Index::new(
                        "idx-auth_group_permission-group_id",
                        ["group_id"],
                    )),
            )
            .create_table(
                Table::new("auth_identity")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("user_id", Type::Integer))
                    .column(Column::new("provider", Type::String(None)))
                    .column(Column::new("subject", Type::String(None)))
                    .column(Column::new("email", Type::String(None)).null())
                    .column(Column::new("date_linked", Type::TimestampTz))
                    .column(Column::new("last_used", Type::TimestampTz).null())
                    .index(Index::new("idx-auth_identity-user_id", ["user_id"]))
                    .index(
                        Index::new(
                            "idx-auth_identity-auth_identity_provider_subject",
                            ["provider", "subject"],
                        )
                        .unique(),
                    ),
            )
            .create_table(
                Table::new("auth_recovery_code")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("user_id", Type::Integer))
                    .column(Column::new("digest", Type::String(None)))
                    .index(Index::new("idx-auth_recovery_code-user_id", ["user_id"])),
            )
            .create_table(
                Table::new("auth_user")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("username", Type::String(None)).unique())
                    .column(Column::new("password_hash", Type::String(None)))
                    .column(Column::new("totp_secret", Type::String(None)).null())
                    .column(Column::new("is_active", Type::Boolean))
                    .column(Column::new("is_staff", Type::Boolean))
                    .column(Column::new("is_superuser", Type::Boolean).default(false))
                    .column(Column::new("totp_last_step", Type::BigInteger).null())
                    .column(Column::new("email", Type::String(None)).null())
                    .column(Column::new("first_name", Type::String(None)).null())
                    .column(Column::new("last_name", Type::String(None)).null())
                    .column(Column::new("phone", Type::String(None)).null())
                    .column(Column::new("last_login", Type::TimestampTz).null())
                    .column(Column::new("date_joined", Type::TimestampTz).null())
                    .index(Index::new("idx-auth_user-auth_user_email", ["email"]).unique()),
            )
            .create_table(
                Table::new("auth_user_group")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("user_id", Type::Integer))
                    .column(Column::new("group_id", Type::Integer))
                    .index(Index::new("idx-auth_user_group-user_id", ["user_id"]))
                    .index(Index::new("idx-auth_user_group-group_id", ["group_id"])),
            )
            .create_table(
                Table::new("auth_user_permission")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("user_id", Type::Integer))
                    .column(Column::new("model", Type::String(None)))
                    .column(Column::new("action", Type::String(None)))
                    .index(Index::new("idx-auth_user_permission-user_id", ["user_id"])),
            ),
        // Added in 0.1: API keys.
        Migration::new("0002_api_keys").create_table(
            Table::new("auth_api_key")
                .column(
                    Column::new("id", Type::Integer)
                        .primary_key()
                        .auto_increment(),
                )
                .column(Column::new("user_id", Type::Integer))
                .column(Column::new("name", Type::String(None)))
                .column(Column::new("prefix", Type::String(None)))
                .column(Column::new("digest", Type::String(None)).unique())
                .column(Column::new("read_only", Type::Boolean))
                .column(Column::new("created", Type::TimestampTz))
                .column(Column::new("last_used", Type::TimestampTz).null())
                .column(Column::new("expires", Type::TimestampTz).null())
                .index(Index::new("idx-auth_api_key-user_id", ["user_id"])),
        ),
    ]
}

/// The upload tables, used when uploads are configured.
pub(crate) fn uploads() -> Vec<Migration> {
    vec![
        Migration::new("0001_initial")
            .create_table(
                Table::new("storage_file")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("key", Type::String(None)).unique())
                    .column(Column::new("filename", Type::String(None)))
                    .column(Column::new("content_type", Type::String(None)))
                    .column(Column::new("size", Type::BigInteger))
                    .column(Column::new("created_by", Type::String(None))),
            )
            .create_table(
                Table::new("storage_upload")
                    .column(Column::new("id", Type::String(None)).primary_key())
                    .column(Column::new("key", Type::String(None)))
                    .column(Column::new("filename", Type::String(None)))
                    .column(Column::new("content_type", Type::String(None)))
                    .column(Column::new("size", Type::BigInteger))
                    .column(Column::new("received", Type::BigInteger))
                    .column(Column::new("parts", Type::Text))
                    .column(Column::new("created_by", Type::String(None))),
            ),
    ]
}

pub(crate) fn auth_tables() -> Result<Vec<EntityTable>, String> {
    use crate::users::{
        api_key, group, group_permission, identity, membership, recovery_code, user,
        user_permission,
    };
    Ok(vec![
        table_of::<identity::Entity>()?,
        table_of::<recovery_code::Entity>()?,
        table_of::<user::Entity>()?,
        table_of::<group::Entity>()?,
        table_of::<membership::Entity>()?,
        table_of::<user_permission::Entity>()?,
        table_of::<group_permission::Entity>()?,
        table_of::<api_key::Entity>()?,
    ])
}

pub(crate) fn upload_tables() -> Result<Vec<EntityTable>, String> {
    use crate::uploads::{file, session};
    Ok(vec![
        table_of::<file::Entity>()?,
        table_of::<session::Entity>()?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Never;

    impl Questions for Never {
        fn renamed_table(&mut self, _: &str, _: &str) -> bool {
            false
        }
        fn renamed_column(&mut self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        fn fill(&mut self, table: &str, column: &Column) -> Result<Value, String> {
            Err(format!("{table}.{} needs a value", column.name))
        }
    }

    fn missing(migrations: &[Migration], tables: &[EntityTable]) -> Vec<String> {
        let state = State::after(migrations).unwrap();
        describe(&diff(&state, tables, &mut Never).unwrap())
    }

    #[test]
    fn built_in_migrations_build_the_built_in_models() {
        assert_eq!(
            missing(&auth(), &auth_tables().unwrap()),
            Vec::<String>::new()
        );
        assert_eq!(
            missing(&uploads(), &upload_tables().unwrap()),
            Vec::<String>::new()
        );
    }

    /// Prints a migration for whatever the built-in models have that the migrations lack:
    /// `cargo test --lib print_built_in -- --ignored --nocapture`.
    #[test]
    #[ignore = "a tool for writing built-in migrations"]
    fn print_built_in() {
        for (name, migrations, tables) in [
            ("auth", auth(), auth_tables().unwrap()),
            ("uploads", uploads(), upload_tables().unwrap()),
        ] {
            let state = State::after(&migrations).unwrap();
            let ops = diff(&state, &tables, &mut Never).unwrap();
            println!("// {name}\n{}", render_migration("0001_initial", &ops));
        }
    }
}
