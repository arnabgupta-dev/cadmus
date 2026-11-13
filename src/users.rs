//! Built-in accounts: the `auth_user` table, a [`UserStore`] over it, and the functions behind
//! the `createsuperuser` and `changepassword` commands.

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, SqlErr,
};

use crate::auth::{AdminUser, UserStore, hash_password_async};
use crate::error::AdminError;

/// Shortest password the built-in commands accept.
const MIN_PASSWORD_CHARS: usize = 8;

/// The `auth_user` entity.
pub mod user {
    use sea_orm::entity::prelude::*;

    /// One account.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_user")]
    pub struct Model {
        /// Primary key.
        #[sea_orm(primary_key)]
        pub id: i32,
        /// Login name.
        #[sea_orm(unique)]
        pub username: String,
        /// Argon2 hash in PHC string format.
        pub password_hash: String,
        /// Base32 TOTP secret, if the account uses a second factor.
        pub totp_secret: Option<String>,
        /// Inactive accounts cannot log in.
        pub is_active: bool,
        /// Only staff may use the admin and the API.
        pub is_staff: bool,
    }

    /// No relations.
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// [`UserStore`] backed by the `auth_user` table. Only active staff accounts are returned.
#[derive(Clone)]
pub struct DbUserStore {
    db: DatabaseConnection,
}

impl DbUserStore {
    /// Reads accounts from `db`.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserStore for DbUserStore {
    async fn find(&self, username: &str) -> Result<Option<AdminUser>, AdminError> {
        let account = user::Entity::find()
            .filter(user::Column::Username.eq(username))
            .one(&self.db)
            .await?;
        Ok(account
            .filter(|a| a.is_active && a.is_staff)
            .map(|a| AdminUser {
                username: a.username,
                password_hash: a.password_hash,
                totp_secret: a.totp_secret,
            }))
    }
}

fn bad(field: &str, message: &str) -> AdminError {
    AdminError::BadInput {
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

fn check_password(password: &str) -> Result<(), AdminError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(bad(
            "password",
            &format!("must be at least {MIN_PASSWORD_CHARS} characters"),
        ));
    }
    Ok(())
}

/// Creates an active staff account.
///
/// # Errors
/// Returns [`AdminError::BadInput`] if the username is empty or taken, or the password is too
/// short; [`AdminError::Db`] on other database errors.
pub async fn create_superuser(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> Result<(), AdminError> {
    let valid_name = !username.is_empty()
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'));
    if !valid_name {
        return Err(bad("username", "use letters, digits and any of `_ - . @`"));
    }
    check_password(password)?;
    let account = user::ActiveModel {
        username: Set(username.to_owned()),
        password_hash: Set(hash_password_async(password).await?),
        totp_secret: Set(None),
        is_active: Set(true),
        is_staff: Set(true),
        ..Default::default()
    };
    match account.insert(db).await {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            Err(bad("username", "an account with this name already exists"))
        }
        Err(error) => Err(error.into()),
    }
}

/// Replaces an account's password.
///
/// # Errors
/// Returns [`AdminError::NotFound`] if there is no such account, [`AdminError::BadInput`] if
/// the password is too short.
pub async fn change_password(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> Result<(), AdminError> {
    check_password(password)?;
    let account = user::Entity::find()
        .filter(user::Column::Username.eq(username))
        .one(db)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let mut account: user::ActiveModel = account.into();
    account.password_hash = Set(hash_password_async(password).await?);
    account.update(db).await?;
    Ok(())
}
