//! `/api/auth/me/...`: what the admin's account pages offer, for apps. People change their own
//! profile, password and two-factor login, and see their linked sites and API keys.
//!
//! These need a login (not an API key). Changes that outlive the login (turning two-factor on,
//! making an API key) also need a recent login or the password, as in the admin.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::IntoResponse;
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AdminState;
use crate::auth::{self, Session};
use crate::error::{AdminError, ApiResult};
use crate::login_api::TokenPair;
use crate::users;

type Shared = State<Arc<AdminState>>;

/// The built-in accounts, and the account this request is logged in as.
async fn own(
    state: &AdminState,
    session: &Session,
) -> Result<(DatabaseConnection, users::user::Model), AdminError> {
    session.require_bearer()?;
    session.require_login()?;
    let db = state.accounts.clone().ok_or_else(|| {
        AdminError::NotFound("managing accounts needs the built-in accounts".into())
    })?;
    let account = users::find_account(&db, &session.username)
        .await?
        .ok_or(AdminError::Unauthorized)?;
    Ok((db, account))
}

/// A recent login, or else the password, proves the person is asking.
async fn proven(
    state: &AdminState,
    session: &Session,
    password: Option<&str>,
) -> Result<(), AdminError> {
    if session.is_fresh()
        || auth::confirm_password(state, &session.username, password.unwrap_or_default()).await?
    {
        return Ok(());
    }
    Err(AdminError::custom(
        StatusCode::FORBIDDEN,
        "reauthentication_required",
        "send your password, or log in again and do this within 15 minutes",
    ))
}

fn limited(state: &AdminState, session: &Session) -> Result<(), AdminError> {
    state
        .limiter
        .attempt(&session.username, None)
        .map_err(|retry_after_secs| AdminError::RateLimited { retry_after_secs })
}

fn wrong_code() -> AdminError {
    AdminError::BadInput {
        field: "code".into(),
        message: "that code is not right; use the current one from your authenticator app".into(),
    }
}

fn no_store(body: Value) -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], Json(body))
}

// ---- profile ---------------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct Profile {
    first_name: Option<String>,
    last_name: Option<String>,
    phone: Option<String>,
}

/// `PATCH /api/auth/me`: changes the name and phone number. The e-mail address is not
/// changed here: logins through other sites and login links rely on it.
pub(crate) async fn update(
    State(state): Shared,
    session: Session,
    Json(body): Json<Profile>,
) -> ApiResult<StatusCode> {
    let (db, account) = own(&state, &session).await?;
    users::set_profile(
        &db,
        &account.username,
        body.first_name,
        body.last_name,
        body.phone,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- password --------------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct PasswordChange {
    current: String,
    password: String,
}

/// `POST /api/auth/me/password`: a new password. Every login made with the old one ends,
/// this one included, so the answer carries new tokens.
pub(crate) async fn password(
    State(state): Shared,
    session: Session,
    Json(body): Json<PasswordChange>,
) -> ApiResult<Json<TokenPair>> {
    if !state.auth.methods.password {
        return Err(AdminError::NotFound("password logins are turned off".into()).into());
    }
    let (db, account) = own(&state, &session).await?;
    limited(&state, &session)?;
    if !auth::confirm_password(&state, &account.username, &body.current).await? {
        return Err(AdminError::BadInput {
            field: "current".into(),
            message: "the current password is not correct".into(),
        }
        .into());
    }
    state.limiter.succeeded(&account.username, None);
    users::change_password(&db, &account.username, &body.password).await?;
    tracing::info!(username = %account.username, "password changed by its owner, through the API");
    let user = state
        .users
        .find(&account.username)
        .await?
        .ok_or(AdminError::Unauthorized)?;
    Ok(Json(crate::login_api::issue(&state, &user).await?))
}

// ---- two-factor ------------------------------------------------------------------------------

/// `POST /api/auth/me/totp`: a new authenticator key to scan, not stored until a code from it
/// is confirmed at `POST /api/auth/me/totp/confirm`.
pub(crate) async fn totp_setup(
    State(state): Shared,
    session: Session,
) -> ApiResult<impl IntoResponse> {
    let (_db, account) = own(&state, &session).await?;
    if account.totp_secret.is_some() {
        return Err(AdminError::Conflict("two-factor login is already on".into()).into());
    }
    let secret = auth::new_totp_secret();
    let uri = auth::totp_uri(&state.settings.site_title, &account.username, &secret);
    Ok(no_store(json!({ "secret": secret, "uri": uri })))
}

#[derive(Deserialize)]
pub(crate) struct TotpConfirm {
    secret: String,
    code: String,
    password: Option<String>,
}

/// `POST /api/auth/me/totp/confirm`: turns two-factor login on with the key from
/// `POST /api/auth/me/totp` and a current code from it. Answers with the recovery codes,
/// shown this once.
pub(crate) async fn totp_confirm(
    State(state): Shared,
    session: Session,
    Json(body): Json<TotpConfirm>,
) -> ApiResult<impl IntoResponse> {
    let (db, account) = own(&state, &session).await?;
    if account.totp_secret.is_some() {
        return Err(AdminError::Conflict("two-factor login is already on".into()).into());
    }
    let secret = body.secret.trim();
    if !auth::totp_secret_is_strong(secret) {
        return Err(AdminError::BadInput {
            field: "secret".into(),
            message: "not a key from POST /api/auth/me/totp".into(),
        }
        .into());
    }
    proven(&state, &session, body.password.as_deref()).await?;
    let step = auth::totp_step_now(secret, body.code.trim()).ok_or_else(wrong_code)?;
    users::store_totp(&db, &account.username, Some(secret.to_owned()), Some(step)).await?;
    let codes = users::new_recovery_codes(&db, &account.username).await?;
    tracing::info!(username = %account.username, "two-factor login turned on, through the API");
    Ok(no_store(json!({ "recovery_codes": codes })))
}

#[derive(Deserialize)]
pub(crate) struct CodeOnly {
    code: String,
}

/// `POST /api/auth/me/totp/disable`: turns two-factor login off, with a current code or a
/// recovery code.
pub(crate) async fn totp_disable(
    State(state): Shared,
    session: Session,
    Json(body): Json<CodeOnly>,
) -> ApiResult<StatusCode> {
    if state.auth.totp_required {
        return Err(AdminError::Forbidden("this site requires two-factor login".into()).into());
    }
    let (db, account) = own(&state, &session).await?;
    let Some(secret) = account.totp_secret else {
        return Ok(StatusCode::NO_CONTENT);
    };
    limited(&state, &session)?;
    if !auth::second_factor(&state, &account.username, &secret, body.code.trim()).await? {
        return Err(wrong_code().into());
    }
    state.limiter.succeeded(&account.username, None);
    users::store_totp(&db, &account.username, None, None).await?;
    tracing::info!(username = %account.username, "two-factor login turned off, through the API");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/auth/me/recovery-codes`: a new set of recovery codes, replacing the old ones,
/// for a current code from the authenticator app.
pub(crate) async fn recovery_codes(
    State(state): Shared,
    session: Session,
    Json(body): Json<CodeOnly>,
) -> ApiResult<impl IntoResponse> {
    let (db, account) = own(&state, &session).await?;
    let Some(secret) = account.totp_secret else {
        return Err(AdminError::Conflict("two-factor login is off".into()).into());
    };
    limited(&state, &session)?;
    let fresh = match auth::totp_step_now(&secret, body.code.trim()) {
        Some(step) => {
            state
                .users
                .accept_totp_step(&account.username, step)
                .await?
        }
        None => false,
    };
    if !fresh {
        return Err(wrong_code().into());
    }
    state.limiter.succeeded(&account.username, None);
    let codes = users::new_recovery_codes(&db, &account.username).await?;
    Ok(no_store(json!({ "recovery_codes": codes })))
}

// ---- linked sites ----------------------------------------------------------------------------

/// `GET /api/auth/me/identities`: the other sites this account logs in through.
pub(crate) async fn identities(State(state): Shared, session: Session) -> ApiResult<Json<Value>> {
    let (db, account) = own(&state, &session).await?;
    let links = users::user_identities(&db, account.id).await?;
    let data: Vec<Value> = links
        .into_iter()
        .map(|link| {
            let label = state
                .auth
                .methods
                .provider(&link.provider)
                .map_or_else(|| link.provider.clone(), |p| p.label.clone());
            json!({
                "id": link.id,
                "provider": link.provider,
                "label": label,
                "email": link.email,
                "linked": link.date_linked,
                "last_used": link.last_used,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

/// `DELETE /api/auth/me/identities/{id}`: forgets a link; the next login through that site
/// is matched by e-mail address again.
pub(crate) async fn identity_delete(
    State(state): Shared,
    session: Session,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let (db, account) = own(&state, &session).await?;
    users::unlink_identity(&db, account.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- API keys --------------------------------------------------------------------------------

fn keys_enabled(state: &AdminState) -> Result<(), AdminError> {
    if state.auth.api_keys {
        Ok(())
    } else {
        Err(AdminError::NotFound("API keys are turned off".into()))
    }
}

/// `GET /api/auth/me/api-keys`: this account's keys; the keys themselves are never shown again.
pub(crate) async fn api_keys(State(state): Shared, session: Session) -> ApiResult<Json<Value>> {
    keys_enabled(&state)?;
    let (db, account) = own(&state, &session).await?;
    let data: Vec<Value> = users::api_keys(&db, account.id)
        .await?
        .into_iter()
        .map(|key| {
            json!({
                "id": key.id,
                "name": key.name,
                "prefix": key.prefix,
                "read_only": key.read_only,
                "created": key.created,
                "last_used": key.last_used,
                "expires": key.expires,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

#[derive(Deserialize)]
pub(crate) struct NewKey {
    name: String,
    /// 30, 90 or 365; `null` for a key that never expires. 90 when left out.
    #[serde(default = "ninety")]
    expires_in_days: Option<u32>,
    #[serde(default)]
    read_only: bool,
    password: Option<String>,
}

#[expect(clippy::unnecessary_wraps, reason = "serde's default needs this type")]
fn ninety() -> Option<u32> {
    Some(90)
}

/// `POST /api/auth/me/api-keys`: makes a key, shown in this answer only.
pub(crate) async fn api_key_create(
    State(state): Shared,
    session: Session,
    Json(body): Json<NewKey>,
) -> ApiResult<impl IntoResponse> {
    keys_enabled(&state)?;
    let (db, account) = own(&state, &session).await?;
    if body
        .expires_in_days
        .is_some_and(|days| ![30, 90, 365].contains(&days))
    {
        return Err(AdminError::BadInput {
            field: "expires_in_days".into(),
            message: "must be 30, 90, 365 or null (never)".into(),
        }
        .into());
    }
    // A key outlives the login, so a stolen token alone must not be enough to make one.
    proven(&state, &session, body.password.as_deref()).await?;
    let key = users::new_api_key(
        &db,
        &account.username,
        &body.name,
        body.expires_in_days,
        body.read_only,
    )
    .await?;
    tracing::info!(username = %account.username, name = body.name.trim(), "API key created, through the API");
    Ok((
        StatusCode::CREATED,
        no_store(json!({ "key": key, "name": body.name.trim(), "read_only": body.read_only })),
    ))
}

/// `DELETE /api/auth/me/api-keys/{id}`: revokes one of this account's keys.
pub(crate) async fn api_key_revoke(
    State(state): Shared,
    session: Session,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    keys_enabled(&state)?;
    let (db, account) = own(&state, &session).await?;
    if !users::revoke_api_key(&db, account.id, id).await? {
        return Err(AdminError::NotFound(format!("API key `{id}`")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}
