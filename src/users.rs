//! Built-in accounts: the `auth_*` tables, a [`UserStore`] over them, and the functions behind
//! the account commands and the Users and Groups pages.
//!
//! The model follows Django's: an account is active or not, staff or not, and possibly a
//! superuser. Staff who are not superusers can do what their own permissions and those of
//! their groups allow.

use async_trait::async_trait;
use sea_orm::schema::SchemaBuilder;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    QueryFilter, QueryOrder, Set, SqlErr, TransactionTrait,
};

use crate::access::{Access, Action};
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
        /// Superusers may do everything, including managing accounts.
        // The default lets `migrate` add the column to a table that already has accounts.
        #[sea_orm(default_value = false)]
        pub is_superuser: bool,
        /// Time step of the last authenticator code this account logged in with; a code for
        /// this step or an earlier one is refused, so each code works once.
        pub totp_last_step: Option<i64>,
    }

    /// No relations.
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The `auth_group` entity: a named set of permissions that accounts can be put in.
pub mod group {
    use sea_orm::entity::prelude::*;

    /// One group.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_group")]
    pub struct Model {
        /// Primary key.
        #[sea_orm(primary_key)]
        pub id: i32,
        /// Display name.
        #[sea_orm(unique)]
        pub name: String,
    }

    /// No relations.
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Which accounts are in which groups.
pub(crate) mod membership {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_user_group")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub user_id: i32,
        #[sea_orm(indexed)]
        pub group_id: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Permissions given to one account directly.
pub(crate) mod user_permission {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_user_permission")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub user_id: i32,
        /// Model slug.
        pub model: String,
        /// An [`crate::Action`] name.
        pub action: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Permissions given to everyone in a group.
pub(crate) mod group_permission {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_group_permission")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub group_id: i32,
        /// Model slug.
        pub model: String,
        /// An [`crate::Action`] name.
        pub action: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Registers every account table with `migrate`.
pub(crate) fn register_schema(builder: SchemaBuilder) -> SchemaBuilder {
    builder
        .register(user::Entity)
        .register(group::Entity)
        .register(membership::Entity)
        .register(user_permission::Entity)
        .register(group_permission::Entity)
}

/// Adds stored `(model, action)` pairs to `access`, ignoring action names this version does
/// not know.
fn with_grants(access: Access, rows: impl IntoIterator<Item = (String, String)>) -> Access {
    rows.into_iter().fold(access, |access, (model, action)| {
        match Action::parse(&action) {
            Some(action) => access.grant(model, action),
            None => access,
        }
    })
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

    /// One conditional update, so two logins racing with the same code cannot both win.
    async fn accept_totp_step(&self, username: &str, step: u64) -> Result<bool, AdminError> {
        let step = i64::try_from(step).unwrap_or(i64::MAX);
        let updated = user::Entity::update_many()
            .col_expr(user::Column::TotpLastStep, Expr::value(step))
            .filter(user::Column::Username.eq(username))
            .filter(
                Condition::any()
                    .add(user::Column::TotpLastStep.is_null())
                    .add(user::Column::TotpLastStep.lt(step)),
            )
            .exec(&self.db)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    /// Read on every request, so a change to an account takes effect at once: a deactivated
    /// or deleted account is locked out even while its token is still valid.
    async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
        let account = user::Entity::find()
            .filter(user::Column::Username.eq(username))
            .one(&self.db)
            .await?;
        let Some(account) = account.filter(|a| a.is_active && a.is_staff) else {
            return Ok(None);
        };
        if account.is_superuser {
            return Ok(Some(Access::superuser()));
        }
        let access = user_grants(&self.db, account.id).await?;
        let groups = user_groups(&self.db, account.id).await?;
        if groups.is_empty() {
            return Ok(Some(access));
        }
        let shared = group_permission::Entity::find()
            .filter(group_permission::Column::GroupId.is_in(groups))
            .all(&self.db)
            .await?;
        Ok(Some(with_grants(
            access,
            shared.into_iter().map(|p| (p.model, p.action)),
        )))
    }
}

/// Turns two-factor authentication on for an account, replacing any earlier secret, and
/// returns the new base32 secret to put into an authenticator app (see [`crate::totp_uri`]).
///
/// # Errors
/// Returns [`AdminError::NotFound`] if there is no such account.
pub async fn enable_totp(db: &DatabaseConnection, username: &str) -> Result<String, AdminError> {
    let secret = crate::auth::new_totp_secret();
    store_totp(db, username, Some(secret.clone()), None).await?;
    Ok(secret)
}

/// Turns two-factor authentication off for an account, e.g. after its device was lost.
///
/// # Errors
/// Returns [`AdminError::NotFound`] if there is no such account.
pub async fn disable_totp(db: &DatabaseConnection, username: &str) -> Result<(), AdminError> {
    store_totp(db, username, None, None).await
}

/// Sets or clears an account's TOTP secret. `used_step` is the step of the code that proved
/// the secret works, which must not be accepted again at login.
pub(crate) async fn store_totp(
    db: &DatabaseConnection,
    username: &str,
    secret: Option<String>,
    used_step: Option<u64>,
) -> Result<(), AdminError> {
    let account = find_account(db, username)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let mut account: user::ActiveModel = account.into();
    account.totp_secret = Set(secret);
    account.totp_last_step = Set(used_step.and_then(|step| i64::try_from(step).ok()));
    account.update(db).await?;
    Ok(())
}

/// The account with this login name, whatever its status.
pub(crate) async fn find_account(
    db: &DatabaseConnection,
    username: &str,
) -> Result<Option<user::Model>, AdminError> {
    Ok(user::Entity::find()
        .filter(user::Column::Username.eq(username))
        .one(db)
        .await?)
}

/// Permissions given to an account directly (not through its groups).
pub(crate) async fn user_grants(
    db: &impl ConnectionTrait,
    user_id: i32,
) -> Result<Access, AdminError> {
    let rows = user_permission::Entity::find()
        .filter(user_permission::Column::UserId.eq(user_id))
        .all(db)
        .await?;
    Ok(with_grants(
        Access::none(),
        rows.into_iter().map(|p| (p.model, p.action)),
    ))
}

/// Ids of the groups an account is in.
pub(crate) async fn user_groups(
    db: &impl ConnectionTrait,
    user_id: i32,
) -> Result<Vec<i32>, AdminError> {
    let rows = membership::Entity::find()
        .filter(membership::Column::UserId.eq(user_id))
        .all(db)
        .await?;
    Ok(rows.into_iter().map(|m| m.group_id).collect())
}

/// Permissions of a group.
pub(crate) async fn group_grants(
    db: &impl ConnectionTrait,
    group_id: i32,
) -> Result<Access, AdminError> {
    let rows = group_permission::Entity::find()
        .filter(group_permission::Column::GroupId.eq(group_id))
        .all(db)
        .await?;
    Ok(with_grants(
        Access::none(),
        rows.into_iter().map(|p| (p.model, p.action)),
    ))
}

/// All groups, by name.
pub(crate) async fn all_groups(db: &DatabaseConnection) -> Result<Vec<group::Model>, AdminError> {
    Ok(group::Entity::find()
        .order_by_asc(group::Column::Name)
        .all(db)
        .await?)
}

/// What the account form submits.
pub(crate) struct AccountInput {
    pub(crate) username: String,
    /// `None` keeps the current password.
    pub(crate) password: Option<String>,
    pub(crate) is_active: bool,
    pub(crate) is_staff: bool,
    pub(crate) is_superuser: bool,
    pub(crate) groups: Vec<i32>,
    pub(crate) grants: Vec<(String, Action)>,
    /// Turns the account's two-factor authentication off.
    pub(crate) reset_totp: bool,
}

/// Creates (`id == None`) or updates an account together with its groups and permissions.
///
/// # Errors
/// [`AdminError::BadInput`] for a bad or taken username or a short or missing password,
/// [`AdminError::NotFound`] if `id` names no account.
pub(crate) async fn save_user(
    db: &DatabaseConnection,
    id: Option<i32>,
    input: AccountInput,
) -> Result<i32, AdminError> {
    check_username(&input.username)?;
    let password_hash = match (&input.password, id) {
        (Some(password), _) => {
            check_password(password)?;
            Some(hash_password_async(password).await?)
        }
        (None, Some(_)) => None,
        (None, None) => return Err(bad("password", "a new account needs a password")),
    };

    let txn = db.begin().await?;
    let mut account = match id {
        Some(id) => user::Entity::find_by_id(id)
            .one(&txn)
            .await?
            .ok_or_else(|| AdminError::NotFound(format!("user `{id}`")))?
            .into(),
        None => user::ActiveModel {
            totp_secret: Set(None),
            totp_last_step: Set(None),
            ..Default::default()
        },
    };
    account.username = Set(input.username);
    account.is_active = Set(input.is_active);
    account.is_staff = Set(input.is_staff);
    account.is_superuser = Set(input.is_superuser);
    if let Some(hash) = password_hash {
        account.password_hash = Set(hash);
    }
    if input.reset_totp {
        account.totp_secret = Set(None);
        account.totp_last_step = Set(None);
    }
    let saved = match id {
        Some(_) => account.update(&txn).await,
        None => account.insert(&txn).await,
    };
    let user_id = saved.map_err(taken("username", "an account"))?.id;

    membership::Entity::delete_many()
        .filter(membership::Column::UserId.eq(user_id))
        .exec(&txn)
        .await?;
    for group_id in input.groups {
        membership::ActiveModel {
            user_id: Set(user_id),
            group_id: Set(group_id),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
    user_permission::Entity::delete_many()
        .filter(user_permission::Column::UserId.eq(user_id))
        .exec(&txn)
        .await?;
    for (model, action) in input.grants {
        user_permission::ActiveModel {
            user_id: Set(user_id),
            model: Set(model),
            action: Set(action.as_str().to_owned()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
    txn.commit().await?;
    Ok(user_id)
}

/// Deletes an account with its memberships and permissions.
///
/// # Errors
/// [`AdminError::NotFound`] if there is no such account.
pub(crate) async fn delete_user(db: &DatabaseConnection, id: i32) -> Result<(), AdminError> {
    let txn = db.begin().await?;
    let deleted = user::Entity::delete_by_id(id).exec(&txn).await?;
    if deleted.rows_affected == 0 {
        return Err(AdminError::NotFound(format!("user `{id}`")));
    }
    membership::Entity::delete_many()
        .filter(membership::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    user_permission::Entity::delete_many()
        .filter(user_permission::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Creates (`id == None`) or updates a group and replaces its permissions.
///
/// # Errors
/// [`AdminError::BadInput`] for an empty or taken name, [`AdminError::NotFound`] if `id`
/// names no group.
pub(crate) async fn save_group(
    db: &DatabaseConnection,
    id: Option<i32>,
    name: &str,
    grants: Vec<(String, Action)>,
) -> Result<i32, AdminError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(bad("name", "a group needs a name"));
    }
    let txn = db.begin().await?;
    let mut row: group::ActiveModel = match id {
        Some(id) => group::Entity::find_by_id(id)
            .one(&txn)
            .await?
            .ok_or_else(|| AdminError::NotFound(format!("group `{id}`")))?
            .into(),
        None => <group::ActiveModel as Default>::default(),
    };
    row.name = Set(name.to_owned());
    let saved = match id {
        Some(_) => row.update(&txn).await,
        None => row.insert(&txn).await,
    };
    let group_id = saved.map_err(taken("name", "a group"))?.id;

    group_permission::Entity::delete_many()
        .filter(group_permission::Column::GroupId.eq(group_id))
        .exec(&txn)
        .await?;
    for (model, action) in grants {
        group_permission::ActiveModel {
            group_id: Set(group_id),
            model: Set(model),
            action: Set(action.as_str().to_owned()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
    txn.commit().await?;
    Ok(group_id)
}

/// Deletes a group, its permissions and its memberships (the accounts stay).
///
/// # Errors
/// [`AdminError::NotFound`] if there is no such group.
pub(crate) async fn delete_group(db: &DatabaseConnection, id: i32) -> Result<(), AdminError> {
    let txn = db.begin().await?;
    let deleted = group::Entity::delete_by_id(id).exec(&txn).await?;
    if deleted.rows_affected == 0 {
        return Err(AdminError::NotFound(format!("group `{id}`")));
    }
    membership::Entity::delete_many()
        .filter(membership::Column::GroupId.eq(id))
        .exec(&txn)
        .await?;
    group_permission::Entity::delete_many()
        .filter(group_permission::Column::GroupId.eq(id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Maps a unique-key violation to "this name is taken"; passes other errors through.
fn taken(field: &'static str, what: &'static str) -> impl Fn(sea_orm::DbErr) -> AdminError {
    move |error| {
        if matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
            bad(field, &format!("{what} with this name already exists"))
        } else {
            error.into()
        }
    }
}

fn check_username(username: &str) -> Result<(), AdminError> {
    let valid = !username.is_empty()
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'));
    if valid {
        Ok(())
    } else {
        Err(bad("username", "use letters, digits and any of `_ - . @`"))
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

/// Creates an active superuser: an account that may do everything, including managing other
/// accounts.
///
/// # Errors
/// Returns [`AdminError::BadInput`] if the username is empty or taken, or the password is too
/// short; [`AdminError::Db`] on other database errors.
pub async fn create_superuser(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> Result<(), AdminError> {
    let input = AccountInput {
        username: username.to_owned(),
        password: Some(password.to_owned()),
        is_active: true,
        is_staff: true,
        is_superuser: true,
        groups: Vec::new(),
        grants: Vec::new(),
        reset_totp: false,
    };
    save_user(db, None, input).await.map(|_| ())
}

/// Makes an existing account an active superuser, keeping its password.
///
/// This is the way back in if no superuser is left, and the way to upgrade accounts created
/// before superusers existed.
///
/// # Errors
/// Returns [`AdminError::NotFound`] if there is no such account.
pub async fn promote(db: &DatabaseConnection, username: &str) -> Result<(), AdminError> {
    let account = user::Entity::find()
        .filter(user::Column::Username.eq(username))
        .one(db)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let mut account: user::ActiveModel = account.into();
    account.is_active = Set(true);
    account.is_staff = Set(true);
    account.is_superuser = Set(true);
    account.update(db).await?;
    Ok(())
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
