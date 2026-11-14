//! The generated REST API: every registered model gets list, create, read, replace, patch
//! and delete endpoints with JSON bodies, JSON errors and bearer-token authentication.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_TYPE, LOCATION};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::AdminState;
use crate::access::Action;
use crate::auth::{self, Credentials, Session};
use crate::error::{AdminError, ApiError, ApiResult};
use crate::handlers::{ListParams, list_query, parse_sort, sortable};
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

pub(crate) fn bad_request(message: &str) -> ApiError {
    AdminError::Unsupported(message.to_owned()).into()
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
            ApiError::from(AdminError::BadInput {
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
    /// The name OAuth 2 clients, such as the documentation's login dialog, look for.
    access_token: String,
    token: String,
    token_type: &'static str,
    expires_in: u64,
}

async fn token(State(state): Shared, parts: Parts, body: Bytes) -> ApiResult<Json<TokenResponse>> {
    // A form is what OAuth 2 password logins send; everything else is taken as JSON.
    let is_form = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    let credentials: Credentials = if is_form {
        let field = |name: &str| {
            url::form_urlencoded::parse(&body)
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        match (field("username"), field("password")) {
            (Some(username), Some(password)) => Credentials {
                username,
                password,
                totp: field("totp").filter(|code| !code.is_empty()),
            },
            _ => return Err(bad_request("expected `username` and `password`")),
        }
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| bad_request("expected JSON with `username` and `password`"))?
    };
    let token = auth::login(&state, &credentials, state.auth.client_ip(&parts)).await?;
    Ok(Json(TokenResponse {
        access_token: token.clone(),
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
    Query(raw): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    session.require_bearer()?;
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    if let Some((name, _)) = params.sort.as_deref().map(parse_sort)
        && !sortable(resource).any(|f| f.name == name)
    {
        return Err(AdminError::BadInput {
            field: "sort".into(),
            message: format!("`{name}` is not a column this list can be sorted by"),
        }
        .into());
    }
    let mut query = list_query(&state, resource, &params);
    query.filters = crate::handlers::parse_filters(resource, &raw, true)?;
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
    session.require(&model, Action::Add)?;
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
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    fetch(resource, &model, &id).await
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
    session.require(model, Action::Change)?;
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
    let resource = state.resource(&model)?;
    session.require(&model, Action::Delete)?;
    resource.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
