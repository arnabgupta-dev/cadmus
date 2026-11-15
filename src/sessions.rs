//! Refresh tokens: an app's login, kept going after its short-lived access token expires.
//!
//! Each login starts a *family*. Using a refresh token spends it and gives the next one in the
//! family. Presenting a spent token again means it was copied, so the whole family is ended:
//! whoever holds the current token has to log in again too, and the copy is useless.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::Digest;

use crate::error::AdminError;
use crate::users::refresh_token;

/// Starts every refresh token, so a leaked one is easy to recognise in logs and scanners.
const PREFIX: &str = "cadmus_rt_";

/// A new random token, as the app receives it.
fn new_token() -> String {
    let random: String = rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{PREFIX}{random}")
}

/// What is stored for a token. It is long and random, so a plain hash is enough.
fn digest(token: &str) -> Option<String> {
    let random = token.strip_prefix(PREFIX)?;
    if random.len() != 64 || !random.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digest = sha2::Sha256::digest(format!("cadmus-refresh-token:{token}").as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn expiry(ttl_secs: u64) -> chrono::DateTime<chrono::Utc> {
    let ttl = i64::try_from(ttl_secs).unwrap_or(i64::MAX / 2);
    chrono::Utc::now() + chrono::Duration::seconds(ttl.min(100 * 365 * 24 * 3600))
}

async fn insert(
    db: &DatabaseConnection,
    user_id: i32,
    family: &str,
    password_print: &str,
    ttl_secs: u64,
) -> Result<String, AdminError> {
    let token = new_token();
    let digest = digest(&token).expect("a new token is well formed");
    refresh_token::ActiveModel {
        user_id: Set(user_id),
        family: Set(family.to_owned()),
        digest: Set(digest),
        password_print: Set(password_print.to_owned()),
        created: Set(chrono::Utc::now()),
        expires: Set(expiry(ttl_secs)),
        used: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(token)
}

/// Starts a login: the first refresh token of a new family. `password_print` is the
/// fingerprint of the account's password hash, so a password change ends the login.
pub(crate) async fn start(
    db: &DatabaseConnection,
    user_id: i32,
    password_print: &str,
    ttl_secs: u64,
) -> Result<String, AdminError> {
    // Tidy up this account's expired tokens while we are here.
    refresh_token::Entity::delete_many()
        .filter(refresh_token::Column::UserId.eq(user_id))
        .filter(refresh_token::Column::Expires.lt(chrono::Utc::now()))
        .exec(db)
        .await?;
    let family: String = rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    insert(db, user_id, &family, password_print, ttl_secs).await
}

/// The current, unexpired token with this value. A spent token ends its whole family and
/// gives `None`, as does anything unknown or expired.
pub(crate) async fn find(
    db: &DatabaseConnection,
    token: &str,
) -> Result<Option<refresh_token::Model>, AdminError> {
    let Some(digest) = digest(token.trim()) else {
        return Ok(None);
    };
    let Some(found) = refresh_token::Entity::find()
        .filter(refresh_token::Column::Digest.eq(digest))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if found.used.is_some() {
        tracing::warn!(
            user_id = found.user_id,
            "a spent refresh token was used again; ending that login"
        );
        end_family(db, &found.family).await?;
        return Ok(None);
    }
    if found.expires <= chrono::Utc::now() {
        return Ok(None);
    }
    Ok(Some(found))
}

/// Spends `current` and returns the next token of its family, or `None` if it was spent
/// meanwhile (two refreshes racing with one token): that counts as reuse and ends the family.
pub(crate) async fn rotate(
    db: &DatabaseConnection,
    current: &refresh_token::Model,
    ttl_secs: u64,
) -> Result<Option<String>, AdminError> {
    let spent = refresh_token::Entity::update_many()
        .col_expr(refresh_token::Column::Used, Expr::value(chrono::Utc::now()))
        .filter(refresh_token::Column::Id.eq(current.id))
        .filter(refresh_token::Column::Used.is_null())
        .exec(db)
        .await?;
    if spent.rows_affected != 1 {
        end_family(db, &current.family).await?;
        return Ok(None);
    }
    Ok(Some(
        insert(
            db,
            current.user_id,
            &current.family,
            &current.password_print,
            ttl_secs,
        )
        .await?,
    ))
}

/// Ends every token of a family: a logout, or a login that can no longer continue.
pub(crate) async fn end_family(db: &DatabaseConnection, family: &str) -> Result<(), AdminError> {
    refresh_token::Entity::delete_many()
        .filter(refresh_token::Column::Family.eq(family))
        .exec(db)
        .await?;
    Ok(())
}

/// Ends the login this token belongs to, spent or not. Unknown tokens are ignored.
pub(crate) async fn end(db: &DatabaseConnection, token: &str) -> Result<(), AdminError> {
    let Some(digest) = digest(token.trim()) else {
        return Ok(());
    };
    if let Some(found) = refresh_token::Entity::find()
        .filter(refresh_token::Column::Digest.eq(digest))
        .one(db)
        .await?
    {
        end_family(db, &found.family).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_well_formed_tokens_have_a_digest() {
        let token = new_token();
        assert!(token.starts_with(PREFIX));
        assert_eq!(digest(&token).unwrap().len(), 64);
        assert_ne!(digest(&token), digest(&new_token()));
        assert_eq!(digest("cadmus_rt_short"), None);
        assert_eq!(digest(&token.replacen(PREFIX, "other_", 1)), None);
        assert_eq!(digest(&format!("{PREFIX}{}", "z".repeat(64))), None);
    }
}
