//! Central error handling: one error type, one classification, and the three ways it is
//! shown (JSON for the API, a page for the admin, plain text elsewhere).
//!
//! Handlers return [`AdminError`] (or [`ApiError`] for JSON). [`middleware`] then makes the
//! response fit the part of the site it came from, and also covers what no handler produced:
//! unknown URLs, wrong methods and panics.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::header::{ALLOW, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use minijinja::context;
use sea_orm::SqlErr;
use serde_json::json;

use crate::AdminState;
use crate::auth::Session;

/// Everything that can go wrong while configuring or serving the app.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdminError {
    /// The model or record does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A submitted value could not be converted to the column's type.
    #[error("invalid value for `{field}`: {message}")]
    BadInput {
        /// Column name.
        field: String,
        /// Human-readable reason.
        message: String,
    },
    /// The model uses something this version cannot handle (e.g. a composite primary key).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Missing or invalid configuration (environment variable, builder call).
    #[error("configuration error: {0}")]
    Config(String),
    /// No valid credentials were presented.
    #[error("authentication required")]
    Unauthorized,
    /// The request was authenticated but refused (e.g. CSRF token mismatch).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] sea_orm::DbErr),
    /// The request conflicts with the current state (e.g. an upload chunk at the wrong offset).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The upload exceeds a configured size limit.
    #[error("too large: {0}")]
    TooLarge(String),
    /// Storage backend error.
    #[error(transparent)]
    Storage(#[from] object_store::Error),
    /// I/O error (binding the listener, reading a password).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Template rendering error.
    #[error(transparent)]
    Template(#[from] minijinja::Error),
    /// An error defined by your application; create it with [`AdminError::custom`].
    #[error("{message}")]
    Custom {
        /// HTTP status to answer with.
        status: StatusCode,
        /// Machine-readable code, the `error.code` of a JSON response.
        code: String,
        /// Message shown to the caller.
        message: String,
    },
}

impl AdminError {
    /// An application-specific error with its own status and code.
    ///
    /// ```
    /// use axum::http::StatusCode;
    /// use heraapi::AdminError;
    ///
    /// let error = AdminError::custom(StatusCode::CONFLICT, "out_of_stock", "no items left");
    /// assert_eq!(error.to_string(), "no items left");
    /// ```
    ///
    /// The message is sent to the caller as written, including for `5xx` statuses.
    #[must_use]
    pub fn custom(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Custom {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    /// What the caller is told: status, code and a message without internal details.
    ///
    /// Internal errors (SQL, storage, template paths) are logged here and replaced by a
    /// generic message.
    fn describe(&self) -> (StatusCode, ErrorInfo) {
        let (status, code, message) = match self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            Self::BadInput { message, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_field",
                message.clone(),
            ),
            Self::Unsupported(message) => (StatusCode::BAD_REQUEST, "bad_request", message.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            Self::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message.clone()),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message.clone()),
            Self::TooLarge(message) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "too_large", message.clone())
            }
            Self::Custom {
                status,
                code,
                message,
            } => {
                return (
                    *status,
                    ErrorInfo {
                        code: code.clone(),
                        message: message.clone(),
                        field: None,
                        json: false,
                    },
                );
            }
            // Constraint violations are the caller's to fix; say so without echoing SQL.
            Self::Db(db)
                if matches!(
                    db.sql_err(),
                    Some(
                        SqlErr::UniqueConstraintViolation(_)
                            | SqlErr::ForeignKeyConstraintViolation(_)
                    )
                ) =>
            {
                (
                    StatusCode::CONFLICT,
                    "conflict",
                    "the change conflicts with existing data (unique or foreign key constraint)"
                        .to_owned(),
                )
            }
            Self::Config(_) | Self::Db(_) | Self::Io(_) | Self::Storage(_) | Self::Template(_) => {
                tracing::error!(error = %self, "request failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, ErrorInfo::internal());
            }
        };
        let field = match self {
            Self::BadInput { field, .. } => Some(field.clone()),
            _ => None,
        };
        (
            status,
            ErrorInfo {
                code: code.to_owned(),
                message,
                field,
                json: false,
            },
        )
    }
}

/// Plain text by default; [`crate::Admin::handle_errors`] upgrades it to JSON under the API path and to a page
/// under the admin path.
impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, info) = self.describe();
        info.text(status)
    }
}

/// An [`AdminError`] answered as JSON: `{"error": {"code", "message", "field"}}`.
///
/// Use it as the error type of your own JSON handlers. Anything that converts into
/// [`AdminError`] (database, storage and I/O errors) converts into this with `?`:
///
/// ```ignore
/// async fn stock(State(db): State<DatabaseConnection>) -> ApiResult<Json<Stock>> {
///     let item = item::Entity::find_by_id(1).one(&db).await?
///         .ok_or_else(|| AdminError::NotFound("item".into()))?;
///     if item.left == 0 {
///         return Err(AdminError::custom(StatusCode::CONFLICT, "out_of_stock", "sold out").into());
///     }
///     Ok(Json(Stock { left: item.left }))
/// }
/// ```
#[derive(Debug)]
pub struct ApiError(AdminError);

impl ApiError {
    /// The underlying error.
    #[must_use]
    pub fn into_inner(self) -> AdminError {
        self.0
    }
}

impl<E: Into<AdminError>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, info) = self.0.describe();
        info.json(status)
    }
}

/// Result of a JSON handler.
pub type ApiResult<T> = Result<T, ApiError>;

/// What an error response says, kept on the response so [`middleware`] can render it again
/// in another format.
#[derive(Clone)]
struct ErrorInfo {
    code: String,
    message: String,
    field: Option<String>,
    /// Whether the response body is already JSON.
    json: bool,
}

impl ErrorInfo {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            field: None,
            json: false,
        }
    }

    fn internal() -> Self {
        Self::new("internal", "internal error")
    }

    /// Describes a bodiless `404`/`405`, which is what axum answers when no route matched.
    fn unrouted(response: &Response) -> Option<Self> {
        if response.headers().contains_key(CONTENT_TYPE) {
            return None;
        }
        match response.status() {
            StatusCode::NOT_FOUND => Some(Self::new("not_found", "not found")),
            StatusCode::METHOD_NOT_ALLOWED => {
                Some(Self::new("method_not_allowed", "method not allowed"))
            }
            _ => None,
        }
    }

    fn text(self, status: StatusCode) -> Response {
        let mut response = (status, self.message.clone()).into_response();
        response.extensions_mut().insert(self);
        response
    }

    fn json(mut self, status: StatusCode) -> Response {
        self.json = true;
        let body = Json(json!({
            "error": { "code": self.code, "message": self.message, "field": self.field }
        }));
        let mut response = (status, body).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response.extensions_mut().insert(self);
        response
    }

    /// The error as an admin page; plain text if the page itself cannot be rendered.
    fn page(self, status: StatusCode, state: &AdminState, session: Option<&Session>) -> Response {
        let page = context! {
            status => status.as_u16(),
            title => status.canonical_reason().unwrap_or("Error"),
            message => &self.message,
        };
        match crate::handlers::render(state, "error.html", session, page) {
            Ok(html) => (status, html).into_response(),
            Err(error) => {
                tracing::error!(%error, "could not render the error page");
                self.text(status)
            }
        }
    }
}

#[derive(PartialEq)]
enum Area {
    Api,
    Admin,
    Other,
}

fn under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// Wraps the whole app: turns panics into `500`s and makes every error response match its
/// area. Under the API path errors are always JSON, under the admin path they are pages, and
/// anywhere else they are left as the handler made them.
pub(crate) async fn middleware(
    State(state): State<Arc<AdminState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let area = if under(&path, &state.api_base) {
        Area::Api
    } else if under(&path, &state.base) {
        Area::Admin
    } else {
        Area::Other
    };
    // Kept so that an error page can show who is logged in.
    let headers = (area == Area::Admin).then(|| request.headers().clone());

    let mut response = match AssertUnwindSafe(next.run(request)).catch_unwind().await {
        Ok(response) => response,
        Err(panic) => {
            tracing::error!(%path, panic = panic_message(&*panic), "handler panicked");
            ErrorInfo::internal().text(StatusCode::INTERNAL_SERVER_ERROR)
        }
    };

    let status = response.status();
    let known = response.extensions_mut().remove::<ErrorInfo>();
    let Some(info) = known.or_else(|| ErrorInfo::unrouted(&response)) else {
        return response;
    };
    let allow = response.headers().get(ALLOW).cloned();
    let mut rendered = match (area, headers) {
        (Area::Api, _) if !info.json => info.json(status),
        (Area::Admin, Some(headers)) => {
            // If the account cannot be read, the page is simply shown as to a visitor.
            let session = Session::from_cookie(&headers, &state).await.ok().flatten();
            info.page(status, &state, session.as_ref())
        }
        _ => return response,
    };
    if let Some(allow) = allow {
        rendered.headers_mut().insert(ALLOW, allow);
    }
    rendered
}
