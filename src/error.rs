//! Error type shared by every part of the admin.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Everything that can go wrong while configuring or serving the admin.
#[derive(Debug, thiserror::Error)]
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
}

impl AdminError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadInput { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unsupported(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Config(_) | Self::Db(_) | Self::Io(_) | Self::Storage(_) | Self::Template(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Internal details (SQL, template paths) go to the log, not to the browser.
        let body = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "admin request failed");
            "internal error".to_owned()
        } else {
            self.to_string()
        };
        (status, body).into_response()
    }
}
