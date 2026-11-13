//! The generated REST API: every registered model gets list, create, read, replace, patch
//! and delete endpoints with JSON bodies, JSON errors and bearer-token authentication.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{LOCATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sea_orm::SqlErr;
use serde::Serialize;
use serde_json::json;

use crate::AdminState;
use crate::auth::{self, Credentials, Session};
use crate::error::AdminError;
use crate::handlers::{ListParams, list_query};
use crate::resource::{Input, Resource, Row, WriteMode};

type Shared = State<Arc<AdminState>>;

/// Upper bound for `?per_page=`, so one request cannot ask for a whole table.
const MAX_PER_PAGE: u64 = 100;

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.api_base.clone();
    Router::new()
        .route(&format!("{b}/auth/token"), post(token))
        .route(&format!("{b}/{{model}}"), get(list).post(create))
        .route(
            &format!("{b}/{{model}}/{{id}}"),
            get(read).put(replace).patch(patch).delete(delete),
        )
        .with_state(state)
}

/// An [`AdminError`] rendered as `{"error": {"code", "message", "field"}}`.
pub(crate) struct ApiError(AdminError);

impl From<AdminError> for ApiError {
    fn from(error: AdminError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self.0 {
            AdminError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", self.0.to_string()),
            AdminError::BadInput { message, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_field",
                message.clone(),
            ),
            AdminError::Unsupported(message) => {
                (StatusCode::BAD_REQUEST, "bad_request", message.clone())
            }
            AdminError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized", self.0.to_string())
            }
            AdminError::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message.clone()),
            AdminError::Conflict(message) => (StatusCode::CONFLICT, "conflict", message.clone()),
            AdminError::TooLarge(message) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "too_large", message.clone())
            }
            // Constraint violations are the caller's to fix; say so without echoing SQL.
            AdminError::Db(db)
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
            _ => {
                tracing::error!(error = %self.0, "API request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error".to_owned(),
                )
            }
        };
        let field = match &self.0 {
            AdminError::BadInput { field, .. } => Some(field.as_str()),
            _ => None,
        };
        let body = Json(json!({ "error": { "code": code, "message": message, "field": field } }));
        let mut response = (status, body).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn bad_request(message: &str) -> ApiError {
    ApiError(AdminError::Unsupported(message.to_owned()))
}

fn strip_hidden(resource: &dyn Resource, mut row: Row) -> Row {
    for name in &resource.options().hidden_fields {
        row.remove(name);
    }
    row
}

/// Parses a JSON object body into resource input, refusing fields the caller may not set.
///
/// Unknown and hidden names are reported the same way, so the API does not confirm that a
/// hidden column exists.
fn json_input(resource: &dyn Resource, body: &Bytes, creating: bool) -> ApiResult<Input> {
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(body)
        .map_err(|_| bad_request("the request body must be a JSON object"))?;
    let options = resource.options();
    let mut input = Input::new();
    for (name, value) in object {
        let bad = |message: &str| {
            ApiError(AdminError::BadInput {
                field: name.clone(),
                message: message.to_owned(),
            })
        };
        let field = resource
            .fields()
            .iter()
            .find(|f| f.name == name && !options.hidden_fields.contains(&f.name))
            .ok_or_else(|| bad("unknown field"))?;
        let read_only = !field.editable
            || field.auto
            || (field.primary_key && !creating)
            || options.readonly_fields.contains(&field.name);
        if read_only {
            return Err(bad("this field is read-only"));
        }
        let text = match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(text) => Some(text),
            other => Some(other.to_string()),
        };
        input.insert(name, text);
    }
    Ok(input)
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
    token_type: &'static str,
    expires_in: u64,
}

async fn token(State(state): Shared, body: Bytes) -> ApiResult<Json<TokenResponse>> {
    let credentials: Credentials = serde_json::from_slice(&body)
        .map_err(|_| bad_request("expected JSON with `username` and `password`"))?;
    let token = auth::login(
        &state.auth,
        &state.users,
        &credentials.username,
        &credentials.password,
        credentials.totp.as_deref(),
    )
    .await?;
    Ok(Json(TokenResponse {
        token,
        token_type: "Bearer",
        expires_in: state.auth.token_ttl_secs,
    }))
}

async fn list(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    session.require_bearer()?;
    let resource = state.resource(&model)?.as_ref();
    let mut query = list_query(&state, resource, &params);
    if let Some(per_page) = params.per_page {
        query.per_page = per_page.clamp(1, MAX_PER_PAGE);
    }
    let page = resource.list(&query).await?;
    let data: Vec<Row> = page
        .rows
        .into_iter()
        .map(|row| strip_hidden(resource, row))
        .collect();
    Ok(Json(json!({
        "data": data,
        "meta": { "total": page.total, "page": query.page + 1, "per_page": query.per_page },
    })))
}

async fn fetch(resource: &dyn Resource, model: &str, id: &str) -> ApiResult<Json<Row>> {
    let row = resource
        .get(id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("{model} `{id}`")))?;
    Ok(Json(strip_hidden(resource, row)))
}

async fn create(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    session.require_bearer()?;
    let resource = state.resource(&model)?.as_ref();
    let input = json_input(resource, &body, true)?;
    let Some(row) = resource.create(&input).await? else {
        return Ok(StatusCode::CREATED.into_response());
    };
    let id = resource
        .fields()
        .iter()
        .find(|f| f.primary_key)
        .and_then(|f| row.get(&f.name))
        .map(|v| match v {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    let mut response = (StatusCode::CREATED, Json(strip_hidden(resource, row))).into_response();
    let location =
        id.and_then(|id| HeaderValue::from_str(&format!("{}/{model}/{id}", state.api_base)).ok());
    if let Some(location) = location {
        response.headers_mut().insert(LOCATION, location);
    }
    Ok(response)
}

async fn read(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
) -> ApiResult<Json<Row>> {
    session.require_bearer()?;
    fetch(state.resource(&model)?.as_ref(), &model, &id).await
}

async fn write(
    state: &AdminState,
    session: &Session,
    model: &str,
    id: &str,
    body: &Bytes,
    mode: WriteMode,
) -> ApiResult<Json<Row>> {
    session.require_bearer()?;
    let resource = state.resource(model)?.as_ref();
    let input = json_input(resource, body, false)?;
    resource.update(id, &input, mode).await?;
    fetch(resource, model, id).await
}

async fn replace(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Row>> {
    write(&state, &session, &model, &id, &body, WriteMode::Replace).await
}

async fn patch(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Row>> {
    write(&state, &session, &model, &id, &body, WriteMode::Patch).await
}

async fn delete(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    session.require_bearer()?;
    state.resource(&model)?.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
