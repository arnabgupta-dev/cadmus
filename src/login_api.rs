//! Logging in through the REST API, for apps: every login method ends in a pair of tokens, a
//! short-lived access token for requests and a refresh token that keeps the login going.
//!
//! Any active account may log in here; whether it is staff only decides whether it may also
//! use the admin. Accounts with two-factor login first get a `challenge`, which they trade for
//! the tokens with an authenticator code at `POST /api/auth/totp`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AdminState;
use crate::auth::{AdminUser, Session, fingerprint, second_factor};
use crate::error::{AdminError, ApiError, ApiResult};

type Shared = State<Arc<AdminState>>;

/// What a challenge stands for: a login that passed its first factor.
const PENDING_PURPOSE: &str = "api-pending-login";
/// How long someone may take to type their authenticator code.
const PENDING_SECS: u64 = 300;

/// The tokens of a login through the API.
///
/// Send `access_token` as `Authorization: Bearer <token>` until it expires (`expires_in`
/// seconds), then trade `refresh_token` for a new pair at `POST /api/auth/refresh`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenPair {
    /// Signed login token for API requests.
    pub access_token: String,
    /// Always `"Bearer"`.
    pub token_type: &'static str,
    /// Seconds until `access_token` expires.
    pub expires_in: u64,
    /// Trades for a new pair once; `None` when refresh tokens are off or the accounts are
    /// not the built-in ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Seconds until `refresh_token` expires if it is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_expires_in: Option<u64>,
}

/// How a login through the API ends: the tokens, or a request for the authenticator code.
#[derive(Debug, Clone)]
pub enum LoginOutcome {
    /// Logged in.
    Tokens(TokenPair),
    /// The account uses two-factor login: send this `challenge` with a code from the
    /// authenticator app (or a recovery code) to `POST /api/auth/totp` within 5 minutes.
    TwoFactor {
        /// Proves the first factor passed.
        challenge: String,
    },
}

impl IntoResponse for LoginOutcome {
    fn into_response(self) -> Response {
        match self {
            Self::Tokens(pair) => Json(pair).into_response(),
            Self::TwoFactor { challenge } => (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "code": "totp_required",
                        "message": "enter the code from your authenticator app",
                        "field": null,
                    },
                    "challenge": challenge,
                })),
            )
                .into_response(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Pending {
    sub: String,
    exp: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn totp_setup_required() -> AdminError {
    AdminError::custom(
        StatusCode::FORBIDDEN,
        "totp_setup_required",
        "this site requires two-factor login; ask an administrator to set it up for your account",
    )
}

/// Signs the tokens for an account whose login is complete, and records the login.
pub(crate) async fn issue(state: &AdminState, user: &AdminUser) -> Result<TokenPair, AdminError> {
    let access_token = state.auth.issue(&user.username, &user.password_hash)?;
    let ttl = state.auth.refresh_ttl_secs;
    let refresh_token = match &state.accounts {
        Some(db) if ttl > 0 => match crate::users::find_account(db, &user.username).await? {
            Some(account) => Some(
                crate::sessions::start(db, account.id, &fingerprint(&user.password_hash), ttl)
                    .await?,
            ),
            None => None,
        },
        _ => None,
    };
    state.users.logged_in(&user.username).await?;
    Ok(TokenPair {
        access_token,
        token_type: "Bearer",
        expires_in: state.auth.token_ttl_secs,
        refresh_expires_in: refresh_token.as_ref().map(|_| ttl),
        refresh_token,
    })
}

/// Finishes a login whose first factor is proven (another site vouched for the person, or
/// they used a login link): a challenge if the account has two-factor login, otherwise the
/// tokens. Inactive accounts get [`AdminError::Unauthorized`].
pub(crate) async fn finish(state: &AdminState, username: &str) -> Result<LoginOutcome, AdminError> {
    let Some(user) = state.users.find(username).await? else {
        return Err(AdminError::Unauthorized);
    };
    if user.totp_secret.is_some() {
        let pending = Pending {
            sub: user.username,
            exp: unix_now() + PENDING_SECS,
        };
        return Ok(LoginOutcome::TwoFactor {
            challenge: state.auth.seal(PENDING_PURPOSE, &pending)?,
        });
    }
    if state.auth.totp_required {
        return Err(totp_setup_required());
    }
    Ok(LoginOutcome::Tokens(issue(state, &user).await?))
}

/// `POST /api/auth/token`: username (or e-mail address) and password, and the authenticator
/// code if the account has one. Without the code, an account with two-factor login gets a
/// challenge instead.
pub(crate) async fn password(
    state: &AdminState,
    credentials: &crate::auth::Credentials,
    address: Option<IpAddr>,
) -> Result<LoginOutcome, AdminError> {
    let (user, key) = crate::auth::check_password(state, credentials, address).await?;
    let code = credentials
        .totp
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty());
    match (&user.totp_secret, code) {
        (Some(secret), Some(code)) => {
            if !second_factor(state, &user.username, secret, code).await? {
                return Err(AdminError::Unauthorized);
            }
        }
        (Some(_), None) => return finish(state, &user.username).await,
        (None, _) if state.auth.totp_required => return Err(totp_setup_required()),
        (None, _) => {}
    }
    state.limiter.succeeded(&key, address);
    Ok(LoginOutcome::Tokens(issue(state, &user).await?))
}

#[derive(Deserialize)]
pub(crate) struct CodeBody {
    challenge: String,
    #[serde(alias = "totp")]
    code: String,
}

/// `POST /api/auth/totp`: the second step of any login with two-factor.
pub(crate) async fn totp(
    State(state): Shared,
    parts: Parts,
    Json(body): Json<CodeBody>,
) -> ApiResult<LoginOutcome> {
    let expired = || {
        AdminError::custom(
            StatusCode::UNAUTHORIZED,
            "challenge_expired",
            "that login took too long or is not valid; log in again",
        )
    };
    let pending = state
        .auth
        .open::<Pending>(PENDING_PURPOSE, body.challenge.trim())
        .ok_or_else(expired)?;
    let address = state.auth.client_ip(&parts);
    state
        .limiter
        .attempt(&pending.sub, address)
        .map_err(|retry_after_secs| AdminError::RateLimited { retry_after_secs })?;
    let user = state.users.find(&pending.sub).await?.ok_or_else(expired)?;
    let accepted = match &user.totp_secret {
        Some(secret) => second_factor(&state, &user.username, secret, body.code.trim()).await?,
        // Turned off meanwhile.
        None => !state.auth.totp_required,
    };
    if !accepted {
        tracing::warn!(username = %user.username, "wrong authenticator code for an API login");
        return Err(AdminError::Unauthorized.into());
    }
    state.limiter.succeeded(&user.username, address);
    Ok(LoginOutcome::Tokens(issue(&state, &user).await?))
}

#[derive(Deserialize)]
pub(crate) struct RefreshBody {
    refresh_token: String,
}

fn refresh_refused() -> AdminError {
    AdminError::custom(
        StatusCode::UNAUTHORIZED,
        "invalid_refresh_token",
        "the refresh token is not valid; log in again",
    )
}

/// `POST /api/auth/refresh`: a new pair for a refresh token, which is then spent.
pub(crate) async fn refresh(
    State(state): Shared,
    Json(body): Json<RefreshBody>,
) -> ApiResult<Json<TokenPair>> {
    let db = state.accounts.as_ref().ok_or_else(refresh_refused)?;
    let ttl = state.auth.refresh_ttl_secs;
    if ttl == 0 {
        return Err(refresh_refused().into());
    }
    let current = crate::sessions::find(db, &body.refresh_token)
        .await?
        .ok_or_else(refresh_refused)?;
    // The account must still be able to log in, with the password it logged in with.
    let account = crate::users::user::Entity::find_by_id(current.user_id)
        .one(db)
        .await?;
    let user = match account {
        Some(account) => state.users.find(&account.username).await?,
        None => None,
    };
    let Some(user) = user.filter(|user| {
        crate::auth::constant_time_eq(
            fingerprint(&user.password_hash).as_bytes(),
            current.password_print.as_bytes(),
        )
    }) else {
        crate::sessions::end_family(db, &current.family).await?;
        return Err(refresh_refused().into());
    };
    let next = crate::sessions::rotate(db, &current, ttl)
        .await?
        .ok_or_else(refresh_refused)?;
    Ok(Json(TokenPair {
        access_token: state.auth.issue(&user.username, &user.password_hash)?,
        token_type: "Bearer",
        expires_in: state.auth.token_ttl_secs,
        refresh_token: Some(next),
        refresh_expires_in: Some(ttl),
    }))
}

/// `POST /api/auth/logout`: ends the login the refresh token belongs to. The access token
/// keeps working until it expires, which is why it is short-lived.
pub(crate) async fn logout(
    State(state): Shared,
    Json(body): Json<RefreshBody>,
) -> ApiResult<StatusCode> {
    if let Some(db) = &state.accounts {
        crate::sessions::end(db, &body.refresh_token).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/auth/me`: the account the request is logged in as.
pub(crate) async fn me(
    State(state): Shared,
    session: Session,
) -> ApiResult<Json<serde_json::Value>> {
    session.require_bearer()?;
    let mut me = json!({
        "username": session.username,
        "staff": session.access.is_staff(),
        "superuser": session.access.is_superuser(),
    });
    if let Some(db) = &state.accounts
        && let Some(account) = crate::users::find_account(db, &session.username).await?
    {
        me["id"] = json!(account.id);
        me["email"] = json!(account.email);
        me["first_name"] = json!(account.first_name);
        me["last_name"] = json!(account.last_name);
        me["two_factor"] = json!(account.totp_secret.is_some());
        me["last_login"] = json!(account.last_login);
    }
    Ok(Json(me))
}

/// Hands out logins from your own routes, after you have proven who someone is in a way of
/// your own (a code sent by text message, a partner's single sign-on, ...).
///
/// The account's two-factor login still applies: [`Logins::log_in`] answers with a
/// challenge for accounts that have it, which the app completes at `POST /api/auth/totp`.
///
/// ```ignore
/// async fn sms_login(logins: Logins, Json(body): Json<SmsCode>) -> ApiResult<LoginOutcome> {
///     let username = check_sms_code(&body).await?;
///     Ok(logins.log_in(&username).await?)
/// }
/// ```
#[derive(Clone)]
pub struct Logins {
    state: Arc<AdminState>,
}

impl Logins {
    /// Logs the account in as if it had passed its first factor: the tokens, or a challenge
    /// for an account with two-factor login.
    ///
    /// # Errors
    /// [`AdminError::Unauthorized`] if there is no such active account, and
    /// [`AdminError::Custom`] with code `totp_setup_required` when two-factor login is
    /// required but the account has none.
    pub async fn log_in(&self, username: &str) -> Result<LoginOutcome, AdminError> {
        finish(&self.state, username).await
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Logins {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<crate::auth::Identify>() {
            Some(crate::auth::Identify(state)) => Ok(Self {
                state: Arc::clone(state),
            }),
            None => Err(AdminError::Config(
                "Logins only works in routes given to App::routes (or passed through \
                 Admin::identify)"
                    .into(),
            )
            .into()),
        }
    }
}
