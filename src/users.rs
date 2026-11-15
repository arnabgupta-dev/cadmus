//! Built-in accounts: the `auth_*` tables, a [`UserStore`] over them, and the functions behind
//! the account commands and the Users and Groups pages.
//!
//! An account is active or not, staff or not, and possibly a superuser. Staff who are not superusers can do what their own permissions and those of
//! their groups allow.

use async_trait::async_trait;
use sea_orm::schema::SchemaBuilder;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    QueryFilter, QueryOrder, Set, SqlErr, TransactionTrait,
};

use crate::access::{Access, Action};
use crate::auth::{AdminUser, ApiKeyOwner, UserStore, hash_password_async};
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
        /// Staff may use the admin; every active account may log in through the API.
        pub is_staff: bool,
        /// Superusers may do everything, including managing accounts.
        // The default lets `migrate` add the column to a table that already has accounts.
        #[sea_orm(default_value = false)]
        pub is_superuser: bool,
        /// Time step of the last authenticator code this account logged in with; a code for
        /// this step or an earlier one is refused, so each code works once.
        pub totp_last_step: Option<i64>,
        /// E-mail address, stored in lower case. Unique when set; it can be used to log in.
        // A separate unique index rather than a unique column: SQLite cannot add a unique
        // column to a table that already exists.
        #[sea_orm(unique_key = "auth_user_email")]
        pub email: Option<String>,
        /// Given name.
        pub first_name: Option<String>,
        /// Family name.
        pub last_name: Option<String>,
        /// Telephone number, as entered.
        pub phone: Option<String>,
        /// When the account last logged in.
        pub last_login: Option<DateTimeUtc>,
        /// When the account was created (unknown for accounts older than this column).
        pub date_joined: Option<DateTimeUtc>,
    }

    impl Model {
        /// First and last name, or the username if neither is set.
        #[must_use]
        pub fn display_name(&self) -> String {
            let name = [self.first_name.as_deref(), self.last_name.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if name.is_empty() {
                self.username.clone()
            } else {
                name
            }
        }
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

/// The `auth_identity` entity: which account each login through another site belongs to.
pub mod identity {
    use sea_orm::entity::prelude::*;

    /// One account at another site, linked to one account here.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_identity")]
    pub struct Model {
        /// Primary key.
        #[sea_orm(primary_key)]
        pub id: i32,
        /// The account here.
        #[sea_orm(indexed)]
        pub user_id: i32,
        /// The login method's name, e.g. `google`.
        #[sea_orm(unique_key = "auth_identity_provider_subject")]
        pub provider: String,
        /// The other site's permanent identifier for the person.
        #[sea_orm(unique_key = "auth_identity_provider_subject")]
        pub subject: String,
        /// The address the other site last gave.
        pub email: Option<String>,
        /// When the link was made.
        pub date_linked: DateTimeUtc,
        /// When it was last used to log in.
        pub last_used: Option<DateTimeUtc>,
    }

    /// No relations.
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Unused recovery codes: what stands in for an authenticator code after a lost phone.
pub(crate) mod recovery_code {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_recovery_code")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub user_id: i32,
        /// SHA-256 of the code; the code itself is shown once and never stored.
        pub digest: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// API keys: long-lived credentials for programs calling the REST API as an account.
pub(crate) mod api_key {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_api_key")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub user_id: i32,
        /// What the owner called it, such as "deploy script".
        pub name: String,
        /// The start of the key, shown so its owner can tell keys apart.
        pub prefix: String,
        /// SHA-256 of the key; the key itself is shown once and never stored.
        #[sea_orm(unique)]
        pub digest: String,
        /// Only reads: the key cannot add, change or delete anything.
        pub read_only: bool,
        pub created: DateTimeUtc,
        pub last_used: Option<DateTimeUtc>,
        /// After this the key stops working; `None` for never.
        pub expires: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Refresh tokens: let an app keep a login going after its access token expires.
pub(crate) mod refresh_token {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "auth_refresh_token")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(indexed)]
        pub user_id: i32,
        /// Shared by every token descended from one login: reusing a spent token ends them all.
        #[sea_orm(indexed)]
        pub family: String,
        /// SHA-256 of the token; the token itself is never stored.
        #[sea_orm(unique)]
        pub digest: String,
        /// Fingerprint of the password hash at login: changing the password ends the login.
        pub password_print: String,
        pub created: DateTimeUtc,
        pub expires: DateTimeUtc,
        /// When it was traded for a new token; `None` while it is the current one.
        pub used: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Registers every account table with `migrate`.
pub(crate) fn register_schema(builder: SchemaBuilder) -> SchemaBuilder {
    builder
        .register(identity::Entity)
        .register(recovery_code::Entity)
        .register(api_key::Entity)
        .register(refresh_token::Entity)
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
    /// Accepts the username or the e-mail address. Active accounts only; whether one may
    /// use the admin is in [`UserStore::access`].
    async fn find(&self, login: &str) -> Result<Option<AdminUser>, AdminError> {
        let account = find_by_login(&self.db, login).await?;
        Ok(account.filter(|a| a.is_active).map(|a| AdminUser {
            username: a.username,
            password_hash: a.password_hash,
            totp_secret: a.totp_secret,
        }))
    }

    async fn logged_in(&self, username: &str) -> Result<(), AdminError> {
        user::Entity::update_many()
            .col_expr(user::Column::LastLogin, Expr::value(chrono::Utc::now()))
            .filter(user::Column::Username.eq(username))
            .exec(&self.db)
            .await?;
        Ok(())
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

    /// One conditional delete, so a code cannot be used twice even by racing logins.
    async fn use_recovery_code(&self, username: &str, digest: &str) -> Result<bool, AdminError> {
        let Some(account) = find_account(&self.db, username).await? else {
            return Ok(false);
        };
        let used = recovery_code::Entity::delete_many()
            .filter(recovery_code::Column::UserId.eq(account.id))
            .filter(recovery_code::Column::Digest.eq(digest))
            .exec(&self.db)
            .await?;
        Ok(used.rows_affected == 1)
    }

    async fn api_key(&self, digest: &str) -> Result<Option<ApiKeyOwner>, AdminError> {
        let Some(key) = api_key::Entity::find()
            .filter(api_key::Column::Digest.eq(digest))
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        let now = chrono::Utc::now();
        if key.expires.is_some_and(|expires| expires <= now) {
            return Ok(None);
        }
        let Some(account) = user::Entity::find_by_id(key.user_id).one(&self.db).await? else {
            return Ok(None);
        };
        // Recorded at most once a minute, so busy keys do not write on every request.
        if key
            .last_used
            .is_none_or(|used| now - used > chrono::Duration::seconds(60))
        {
            api_key::Entity::update_many()
                .col_expr(api_key::Column::LastUsed, Expr::value(now))
                .filter(api_key::Column::Id.eq(key.id))
                .exec(&self.db)
                .await?;
        }
        Ok(Some(ApiKeyOwner {
            username: account.username,
            read_only: key.read_only,
        }))
    }

    /// Read on every request, so a change to an account takes effect at once: a deactivated
    /// or deleted account is locked out even while its token is still valid.
    async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
        let account = user::Entity::find()
            .filter(user::Column::Username.eq(username))
            .one(&self.db)
            .await?;
        let Some(account) = account.filter(|a| a.is_active) else {
            return Ok(None);
        };
        let access = if account.is_superuser {
            Access::superuser()
        } else {
            let access = user_grants(&self.db, account.id).await?;
            let groups = user_groups(&self.db, account.id).await?;
            if groups.is_empty() {
                access
            } else {
                let shared = group_permission::Entity::find()
                    .filter(group_permission::Column::GroupId.is_in(groups))
                    .all(&self.db)
                    .await?;
                with_grants(access, shared.into_iter().map(|p| (p.model, p.action)))
            }
        };
        // Accounts that are not staff log in through the API only.
        Ok(Some(if account.is_staff {
            access
        } else {
            access.api_only()
        }))
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
    // Codes belong to one setup; a new secret, or none, retires them.
    recovery_code::Entity::delete_many()
        .filter(recovery_code::Column::UserId.eq(account.id))
        .exec(db)
        .await?;
    let mut account: user::ActiveModel = account.into();
    account.totp_secret = Set(secret);
    account.totp_last_step = Set(used_step.and_then(|step| i64::try_from(step).ok()));
    account.update(db).await?;
    Ok(())
}

/// Replaces an account's two-factor recovery codes with a new set of ten and returns them, to
/// be shown once: each logs in once in place of an authenticator code. What the `totp`
/// command prints after turning two-factor authentication on.
///
/// # Errors
/// [`AdminError::NotFound`] if there is no such account.
pub async fn new_recovery_codes(
    db: &DatabaseConnection,
    username: &str,
) -> Result<Vec<String>, AdminError> {
    let account = find_account(db, username)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let codes = crate::auth::new_recovery_codes();
    let txn = db.begin().await?;
    recovery_code::Entity::delete_many()
        .filter(recovery_code::Column::UserId.eq(account.id))
        .exec(&txn)
        .await?;
    for code in &codes {
        recovery_code::ActiveModel {
            user_id: Set(account.id),
            digest: Set(crate::auth::recovery_digest(code).expect("generated codes are valid")),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
    txn.commit().await?;
    Ok(codes)
}

/// How many unused recovery codes an account has.
pub(crate) async fn recovery_codes_left(
    db: &DatabaseConnection,
    username: &str,
) -> Result<u64, AdminError> {
    use sea_orm::PaginatorTrait;
    let Some(account) = find_account(db, username).await? else {
        return Ok(0);
    };
    Ok(recovery_code::Entity::find()
        .filter(recovery_code::Column::UserId.eq(account.id))
        .count(db)
        .await?)
}

/// The account a login form names: by username, or else by e-mail address. Usernames may
/// contain `@`, so the username is tried first.
pub(crate) async fn find_by_login(
    db: &DatabaseConnection,
    login: &str,
) -> Result<Option<user::Model>, AdminError> {
    let login = login.trim();
    if let Some(account) = find_account(db, login).await? {
        return Ok(Some(account));
    }
    if !login.contains('@') {
        return Ok(None);
    }
    Ok(user::Entity::find()
        .filter(user::Column::Email.eq(login.to_lowercase()))
        .one(db)
        .await?)
}

/// The account with this e-mail address, whatever its status.
pub(crate) async fn find_by_email(
    db: &DatabaseConnection,
    email: &str,
) -> Result<Option<user::Model>, AdminError> {
    Ok(user::Entity::find()
        .filter(user::Column::Email.eq(email.trim().to_lowercase()))
        .one(db)
        .await?)
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
    /// Empty text is stored as no value.
    pub(crate) email: Option<String>,
    pub(crate) first_name: Option<String>,
    pub(crate) last_name: Option<String>,
    pub(crate) phone: Option<String>,
}

/// Lower-cases an address and checks that it looks like one; `None` for empty input.
fn normalise_email(email: Option<&str>) -> Result<Option<String>, AdminError> {
    let Some(email) = email.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let valid = email.len() <= 254
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if !valid {
        return Err(bad("email", "enter an address like name@example.com"));
    }
    Ok(Some(email.to_lowercase()))
}

/// Checks a telephone number loosely: digits with the usual separators.
fn normalise_phone(phone: Option<&str>) -> Result<Option<String>, AdminError> {
    let Some(phone) = phone.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let digits = phone.chars().filter(char::is_ascii_digit).count();
    let valid = (5..=20).contains(&digits)
        && phone
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.'));
    if !valid {
        return Err(bad("phone", "use digits, spaces and + - ( ) only"));
    }
    Ok(Some(phone.to_owned()))
}

fn text(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
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
    let email = normalise_email(input.email.as_deref())?;
    let phone = normalise_phone(input.phone.as_deref())?;
    // Checked here for a clear message; the unique key still guards against a race.
    if let Some(address) = &email {
        let holder = user::Entity::find()
            .filter(user::Column::Email.eq(address.as_str()))
            .one(db)
            .await?;
        if holder.is_some_and(|h| Some(h.id) != id) {
            return Err(bad("email", "another account already uses this address"));
        }
    }
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
            last_login: Set(None),
            date_joined: Set(Some(chrono::Utc::now())),
            ..Default::default()
        },
    };
    account.username = Set(input.username);
    account.email = Set(email);
    account.first_name = Set(text(input.first_name));
    account.last_name = Set(text(input.last_name));
    account.phone = Set(phone);
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
    if input.reset_totp {
        recovery_code::Entity::delete_many()
            .filter(recovery_code::Column::UserId.eq(user_id))
            .exec(&txn)
            .await?;
    }

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
    identity::Entity::delete_many()
        .filter(identity::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    recovery_code::Entity::delete_many()
        .filter(recovery_code::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    api_key::Entity::delete_many()
        .filter(api_key::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    refresh_token::Entity::delete_many()
        .filter(refresh_token::Column::UserId.eq(id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Most API keys one account may have.
pub(crate) const MAX_API_KEYS: u64 = 50;

/// Creates an API key for an account and returns it. It is shown once: only its digest is
/// stored. The key acts as the account, with the account's permissions (only reading, if
/// `read_only`), until it is revoked, `expires_in_days` pass, or the account is deactivated.
/// What the `apikey` command does.
///
/// # Errors
/// [`AdminError::NotFound`] if there is no such account, and [`AdminError::BadInput`] for an
/// empty or overlong name or too many keys.
pub async fn new_api_key(
    db: &DatabaseConnection,
    username: &str,
    name: &str,
    expires_in_days: Option<u32>,
    read_only: bool,
) -> Result<String, AdminError> {
    use sea_orm::PaginatorTrait;
    let account = find_account(db, username)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(AdminError::BadInput {
            field: "name".into(),
            message: "give the key a name of up to 100 characters".into(),
        });
    }
    let existing = api_key::Entity::find()
        .filter(api_key::Column::UserId.eq(account.id))
        .count(db)
        .await?;
    if existing >= MAX_API_KEYS {
        return Err(AdminError::BadInput {
            field: "name".into(),
            message: format!("an account can have at most {MAX_API_KEYS} keys; revoke one first"),
        });
    }
    let (key, prefix) = crate::auth::new_api_key();
    let now = chrono::Utc::now();
    api_key::ActiveModel {
        user_id: Set(account.id),
        name: Set(name.to_owned()),
        prefix: Set(prefix),
        digest: Set(crate::auth::api_key_digest(&key).expect("generated keys are valid")),
        read_only: Set(read_only),
        created: Set(now),
        last_used: Set(None),
        expires: Set(expires_in_days.map(|days| now + chrono::Duration::days(days.into()))),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(key)
}

/// An account's API keys, newest first.
pub(crate) async fn api_keys(
    db: &DatabaseConnection,
    user_id: i32,
) -> Result<Vec<api_key::Model>, AdminError> {
    Ok(api_key::Entity::find()
        .filter(api_key::Column::UserId.eq(user_id))
        .order_by_desc(api_key::Column::Id)
        .all(db)
        .await?)
}

/// Revokes one of an account's API keys; `false` if it has no such key.
pub(crate) async fn revoke_api_key(
    db: &DatabaseConnection,
    user_id: i32,
    key_id: i32,
) -> Result<bool, AdminError> {
    let deleted = api_key::Entity::delete_many()
        .filter(api_key::Column::Id.eq(key_id))
        .filter(api_key::Column::UserId.eq(user_id))
        .exec(db)
        .await?;
    Ok(deleted.rows_affected == 1)
}

/// The other sites an account logs in through, oldest link first.
pub(crate) async fn user_identities(
    db: &DatabaseConnection,
    user_id: i32,
) -> Result<Vec<identity::Model>, AdminError> {
    Ok(identity::Entity::find()
        .filter(identity::Column::UserId.eq(user_id))
        .order_by_asc(identity::Column::Id)
        .all(db)
        .await?)
}

/// Removes one link between an account and another site.
pub(crate) async fn unlink_identity(
    db: &DatabaseConnection,
    user_id: i32,
    identity_id: i32,
) -> Result<(), AdminError> {
    identity::Entity::delete_many()
        .filter(identity::Column::Id.eq(identity_id))
        .filter(identity::Column::UserId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// The account someone who logged in through another site gets, or the message saying why
/// there is none:
///
/// 1. the account this identity was linked to before;
/// 2. otherwise the account with the address the site vouches for, which is then linked;
/// 3. otherwise a new account, if [`crate::settings::Signup`] allows the address: staff
///    when `staff` (a login to the admin), otherwise an account for the API only.
pub(crate) async fn resolve_identity(
    db: &DatabaseConnection,
    methods: &crate::oauth::Methods,
    label: &str,
    found: &crate::oauth::Identity,
    staff: bool,
) -> Result<Result<user::Model, String>, AdminError> {
    let now = chrono::Utc::now();
    let link = identity::Entity::find()
        .filter(identity::Column::Provider.eq(found.provider.as_str()))
        .filter(identity::Column::Subject.eq(found.subject.as_str()))
        .one(db)
        .await?;
    if let Some(link) = link {
        if let Some(account) = user::Entity::find_by_id(link.user_id).one(db).await? {
            let mut link: identity::ActiveModel = link.into();
            link.email = Set(found.email.clone());
            link.last_used = Set(Some(now));
            link.update(db).await?;
            return Ok(Ok(account));
        }
        // The account was removed some other way; forget the link.
        identity::Entity::delete_by_id(link.id).exec(db).await?;
    }

    let Some(email) = found
        .email
        .as_deref()
        .filter(|_| found.email_verified)
        .map(|email| email.trim().to_lowercase())
        .filter(|email| email.contains('@'))
    else {
        return Ok(Err(format!(
            "{label} did not confirm an e-mail address for this account, so it cannot be              matched to an account here."
        )));
    };
    let existing = user::Entity::find()
        .filter(user::Column::Email.eq(email.as_str()))
        .one(db)
        .await?;
    let account = match existing {
        Some(account) => account,
        None if methods.signup_allows(&email) => {
            signup(
                db,
                &email,
                found.first_name.clone(),
                found.last_name.clone(),
                &methods.signup_groups,
                staff,
                &found.provider,
            )
            .await?
        }
        None => {
            return Ok(Err(format!(
                "No account here uses {email}. Ask an administrator to add you."
            )));
        }
    };
    identity::ActiveModel {
        user_id: Set(account.id),
        provider: Set(found.provider.clone()),
        subject: Set(found.subject.clone()),
        email: Set(Some(email)),
        date_linked: Set(now),
        last_used: Set(Some(now)),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(Ok(account))
}

/// Creates the account of someone who logged in through another site or a login link for
/// the first time: active, without permissions of its own, in `groups`, with a password
/// nobody knows. Staff (may use the admin) when `staff`; otherwise it uses the API only.
/// `source` names how they came, for the log.
pub(crate) async fn signup(
    db: &DatabaseConnection,
    email: &str,
    first_name: Option<String>,
    last_name: Option<String>,
    groups: &[String],
    staff: bool,
    source: &str,
) -> Result<user::Model, AdminError> {
    // The address is the username, unless an older account already has that name.
    let mut username = email.to_owned();
    let mut suffix = 2;
    while find_account(db, &username).await?.is_some() {
        username = format!("{email}-{suffix}");
        suffix += 1;
    }
    let unguessable: String = rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let password_hash = hash_password_async(&unguessable).await?;
    let txn = db.begin().await?;
    let account = user::ActiveModel {
        username: Set(username),
        password_hash: Set(password_hash),
        totp_secret: Set(None),
        is_active: Set(true),
        is_staff: Set(staff),
        is_superuser: Set(false),
        totp_last_step: Set(None),
        email: Set(Some(email.to_owned())),
        first_name: Set(first_name),
        last_name: Set(last_name),
        phone: Set(None),
        last_login: Set(None),
        date_joined: Set(Some(chrono::Utc::now())),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    for name in groups {
        match group::Entity::find()
            .filter(group::Column::Name.eq(name.as_str()))
            .one(&txn)
            .await?
        {
            Some(group) => {
                membership::ActiveModel {
                    user_id: Set(account.id),
                    group_id: Set(group.id),
                    ..Default::default()
                }
                .insert(&txn)
                .await?;
            }
            None => {
                tracing::warn!(group = %name, "auth.signup_groups names a group that does not exist")
            }
        }
    }
    txn.commit().await?;
    tracing::info!(username = %account.username, %source, "account created at first login");
    Ok(account)
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

pub(crate) fn check_password(password: &str) -> Result<(), AdminError> {
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
        email: None,
        first_name: None,
        last_name: None,
        phone: None,
    };
    save_user(db, None, input).await.map(|_| ())
}

/// Sets the e-mail address of an account, e.g. after `createsuperuser --email`.
///
/// # Errors
/// [`AdminError::NotFound`] if there is no such account, [`AdminError::BadInput`] if the
/// address is malformed or another account uses it.
pub async fn set_email(
    db: &DatabaseConnection,
    username: &str,
    email: &str,
) -> Result<(), AdminError> {
    let address = normalise_email(Some(email))?
        .ok_or_else(|| bad("email", "enter an address like name@example.com"))?;
    let account = find_account(db, username)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{username}`")))?;
    let holder = user::Entity::find()
        .filter(user::Column::Email.eq(address.as_str()))
        .one(db)
        .await?;
    if holder.is_some_and(|h| h.id != account.id) {
        return Err(bad("email", "another account already uses this address"));
    }
    let mut account: user::ActiveModel = account.into();
    account.email = Set(Some(address));
    account.update(db).await?;
    Ok(())
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
