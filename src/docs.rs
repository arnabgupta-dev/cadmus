//! API documentation: an OpenAPI description generated from the registered models, and the
//! interactive pages that show it (Swagger UI and ReDoc), optionally behind a password.
//!
//! Under the API path: `openapi.json`, `docs` and `redoc`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use minijinja::context;
use serde_json::{Map, Value, json};

use crate::AdminState;
use crate::access::Action;
use crate::auth::constant_time_eq;
use crate::error::AdminError;
use crate::resource::{Field, FieldKind, Resource};
use crate::settings::UiSettings;
use crate::uploads::FILES;

pub(crate) const SWAGGER_JS: &str =
    "https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui-bundle.js";
pub(crate) const SWAGGER_CSS: &str =
    "https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui.css";
pub(crate) const REDOC_JS: &str =
    "https://cdn.jsdelivr.net/npm/redoc@2.1.5/bundles/redoc.standalone.js";

type Extend = Arc<dyn Fn(&mut Value) + Send + Sync>;

/// Settings for the API documentation. Build in code, or load with [`DocsConfig::from_env`].
#[derive(Clone)]
pub struct DocsConfig {
    enabled: bool,
    title: Option<String>,
    version: String,
    description: Option<String>,
    credentials: Option<(String, String)>,
    swagger_js: String,
    swagger_css: String,
    redoc_js: String,
    extend: Option<Extend>,
}

// Hand-written so the password can never reach a log line.
impl std::fmt::Debug for DocsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocsConfig")
            .field("enabled", &self.enabled)
            .field("title", &self.title)
            .field("version", &self.version)
            .field("protected", &self.credentials.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DocsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            title: None,
            version: "0.1.0".to_owned(),
            description: None,
            credentials: None,
            swagger_js: SWAGGER_JS.to_owned(),
            swagger_css: SWAGGER_CSS.to_owned(),
            redoc_js: REDOC_JS.to_owned(),
            extend: None,
        }
    }
}

impl DocsConfig {
    /// Documentation on, open to everyone, titled after the site.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the settings from the environment.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `DOCS_ENABLED` | `false` removes the documentation pages and `openapi.json` (default `true`) |
    /// | `DOCS_USERNAME`, `DOCS_PASSWORD` | Set both to ask for them before showing the documentation (default: open) |
    /// | `DOCS_TITLE` | Title of the API (default: the site title followed by "API") |
    /// | `DOCS_VERSION` | Version of the API shown in the documentation (default `0.1.0`) |
    /// | `DOCS_DESCRIPTION` | Introduction shown at the top; Markdown |
    /// | `DOCS_SWAGGER_JS_URL`, `DOCS_SWAGGER_CSS_URL`, `DOCS_REDOC_JS_URL` | Where the pages load their viewer from, to host it yourself (default: a pinned version on jsDelivr) |
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is invalid, or only one of the username and
    /// password is set.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`DocsConfig::from_env`] but reads from `lookup`.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is invalid.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let set = |key: &str| lookup(key).filter(|value| !value.is_empty());
        let mut config = Self::new();
        if let Some(value) = set("DOCS_ENABLED") {
            config.enabled = match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => {
                    return Err(AdminError::Config(
                        "DOCS_ENABLED must be true or false".into(),
                    ));
                }
            };
        }
        match (set("DOCS_USERNAME"), set("DOCS_PASSWORD")) {
            (Some(username), Some(password)) => config = config.protect(username, password),
            (None, None) => {}
            _ => {
                return Err(AdminError::Config(
                    "set both DOCS_USERNAME and DOCS_PASSWORD, or neither".into(),
                ));
            }
        }
        config.title = set("DOCS_TITLE");
        config.description = set("DOCS_DESCRIPTION");
        if let Some(version) = set("DOCS_VERSION") {
            config.version = version;
        }
        if let Some(url) = set("DOCS_SWAGGER_JS_URL") {
            config.swagger_js = url;
        }
        if let Some(url) = set("DOCS_SWAGGER_CSS_URL") {
            config.swagger_css = url;
        }
        if let Some(url) = set("DOCS_REDOC_JS_URL") {
            config.redoc_js = url;
        }
        Ok(config)
    }

    /// Turns the documentation pages and `openapi.json` on or off.
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Asks for this username and password (HTTP Basic authentication, the browser's own
    /// prompt) before showing the documentation or `openapi.json`.
    ///
    /// These are separate from the accounts: they only open the documentation, and calling
    /// the API from it still needs an account.
    #[must_use]
    pub fn protect(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some((username.into(), password.into()));
        self
    }

    /// Title of the API.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Version of the API, e.g. `env!("CARGO_PKG_VERSION")`.
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Introduction shown at the top of the documentation; Markdown.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Where the pages load Swagger UI and ReDoc from, to host them yourself instead of
    /// using the public CDN.
    #[must_use]
    pub fn assets(
        mut self,
        swagger_js: impl Into<String>,
        swagger_css: impl Into<String>,
        redoc_js: impl Into<String>,
    ) -> Self {
        self.swagger_js = swagger_js.into();
        self.swagger_css = swagger_css.into();
        self.redoc_js = redoc_js.into();
        self
    }

    /// Edits the generated OpenAPI document before it is served, e.g. to describe routes of
    /// your own:
    ///
    /// ```
    /// use cadmus::DocsConfig;
    /// use serde_json::json;
    ///
    /// let docs = DocsConfig::new().extend(|document| {
    ///     document["paths"]["/health"] = json!({
    ///         "get": { "summary": "Health check", "security": [], "responses": { "200": { "description": "ok" } } }
    ///     });
    /// });
    /// ```
    #[must_use]
    pub fn extend(mut self, edit: impl Fn(&mut Value) + Send + Sync + 'static) -> Self {
        self.extend = Some(Arc::new(edit));
        self
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// The documentation as served: its settings and the document, generated once at start-up.
pub(crate) struct Docs {
    config: DocsConfig,
    title: String,
    document: Value,
    json: String,
}

impl Docs {
    pub(crate) fn new(
        config: DocsConfig,
        api_base: &str,
        settings: &UiSettings,
        resources: &[Arc<dyn Resource>],
        uploads: bool,
        auth: &crate::auth::AuthConfig,
        accounts: bool,
    ) -> Self {
        let title = config
            .title
            .clone()
            .unwrap_or_else(|| format!("{} API", settings.site_title));
        let mut document = document(
            &config, &title, api_base, resources, uploads, auth, accounts,
        );
        if let Some(edit) = &config.extend {
            edit(&mut document);
        }
        let json = serde_json::to_string_pretty(&document).unwrap_or_default();
        Self {
            config,
            title,
            document,
            json,
        }
    }

    pub(crate) fn document(&self) -> &Value {
        &self.document
    }
}

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.api_base.clone();
    Router::new()
        .route(&format!("{b}/openapi.json"), get(openapi))
        .route(&format!("{b}/docs"), get(swagger))
        .route(&format!("{b}/redoc"), get(redoc))
        .with_state(state)
}

/// The documentation, if the request may see it. These routes are only mounted when it is
/// enabled.
/// The refusal is boxed only because a whole response is large to pass around by value.
fn open<'a>(state: &'a AdminState, parts: &Parts) -> Result<&'a Docs, Box<Response>> {
    let docs = state
        .docs
        .as_ref()
        .ok_or_else(|| Box::new(AdminError::NotFound("documentation".into()).into_response()))?;
    let Some((username, password)) = &docs.config.credentials else {
        return Ok(docs);
    };
    let challenge = || {
        let mut response = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"API documentation\", charset=\"UTF-8\""),
        );
        Box::new(response)
    };
    let Some(given) = basic_credentials(parts) else {
        return Err(challenge());
    };
    // Guesses are limited per client like logins are; without an address, for everyone.
    let key = match state.auth.client_ip(parts) {
        Some(address) => format!("documentation from {address}"),
        None => "documentation".to_owned(),
    };
    if let Err(retry_after_secs) = state.limiter.attempt(&key, None) {
        return Err(Box::new(
            AdminError::RateLimited { retry_after_secs }.into_response(),
        ));
    }
    // Both halves are always compared, so timing does not say which one was wrong.
    let name_ok = constant_time_eq(given.0.as_bytes(), username.as_bytes());
    let password_ok = constant_time_eq(given.1.as_bytes(), password.as_bytes());
    if name_ok & password_ok {
        state.limiter.succeeded(&key, None);
        Ok(docs)
    } else {
        tracing::warn!("wrong credentials for the API documentation");
        Err(challenge())
    }
}

/// The username and password of an `Authorization: Basic` header.
fn basic_credentials(parts: &Parts) -> Option<(String, String)> {
    let header = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, password) = text.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}

async fn openapi(State(state): State<Arc<AdminState>>, parts: Parts) -> Response {
    match open(&state, &parts) {
        Ok(docs) => ([(CONTENT_TYPE, "application/json")], docs.json.clone()).into_response(),
        Err(refusal) => *refusal,
    }
}

fn page(state: &AdminState, parts: &Parts, template: &str) -> Response {
    let docs = match open(state, parts) {
        Ok(docs) => docs,
        Err(refusal) => return *refusal,
    };
    let rendered = state.templates.get_template(template).and_then(|t| {
        t.render(context! {
            title => &docs.title,
            openapi_url => format!("{}/openapi.json", state.api_base),
            swagger_js => &docs.config.swagger_js,
            swagger_css => &docs.config.swagger_css,
            redoc_js => &docs.config.redoc_js,
        })
    });
    match rendered {
        Ok(html) => Html(html).into_response(),
        Err(error) => AdminError::from(error).into_response(),
    }
}

async fn swagger(State(state): State<Arc<AdminState>>, parts: Parts) -> Response {
    page(&state, &parts, "docs_swagger.html")
}

async fn redoc(State(state): State<Arc<AdminState>>, parts: Parts) -> Response {
    page(&state, &parts, "docs_redoc.html")
}

// ---- the document ------------------------------------------------------------------------

/// `blog_post` -> `BlogPost`.
fn schema_name(slug: &str) -> String {
    slug.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn reference(schema: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{schema}") })
}

fn error_response(name: &str) -> Value {
    json!({ "$ref": format!("#/components/responses/{name}") })
}

fn json_body(schema: &Value) -> Value {
    json!({ "required": true, "content": { "application/json": { "schema": schema } } })
}

fn json_response(description: &str, schema: &Value) -> Value {
    json!({ "description": description, "content": { "application/json": { "schema": schema } } })
}

fn field_schema(field: &Field) -> Value {
    let mut schema = match field.kind {
        FieldKind::Text | FieldKind::LongText => json!({ "type": "string" }),
        FieldKind::Html => json!({
            "type": "string",
            "contentMediaType": "text/html",
            "description": "Formatted text as HTML; scripts, styles and unknown tags are removed when it is saved",
            "example": "<p>Hello <strong>world</strong></p>",
        }),
        FieldKind::Integer => json!({ "type": "integer" }),
        FieldKind::Float => json!({ "type": "number" }),
        FieldKind::Boolean => json!({ "type": "boolean" }),
        // No `format`: `date-time` would demand a time zone, which plain timestamps lack.
        FieldKind::DateTime => json!({
            "type": "string",
            "description": "Date and time, ISO 8601",
            "example": "2026-03-04T05:06:07",
        }),
        FieldKind::Date => json!({ "type": "string", "format": "date" }),
        FieldKind::Time => json!({ "type": "string", "example": "05:06:07" }),
        FieldKind::Json => json!({ "description": "Any JSON value" }),
        FieldKind::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldKind::Decimal => json!({
            "type": "string",
            "description": "An exact decimal number, as text",
            "example": "19.99",
        }),
        FieldKind::Choice => json!({ "type": "string", "enum": field.choices }),
        FieldKind::List => json!({ "type": "array", "items": {} }),
        FieldKind::Other => json!({ "description": "A value the API returns but cannot change" }),
    };
    if field.nullable {
        schema["nullable"] = json!(true);
    }
    if let Some(target) = &field.reference {
        schema["description"] = json!(format!("The `{}` of a `{}`", target.column, target.model));
    }
    schema
}

/// The body of a write: the fields that may be sent, and which of them must be.
fn write_schema(
    resource: &dyn Resource,
    creating: bool,
    required: impl Fn(&Field) -> bool,
) -> Value {
    let options = resource.options();
    let mut properties = Map::new();
    let mut needed = Vec::new();
    for field in resource.fields() {
        let writable = field.editable
            && !field.auto
            && (creating || !field.primary_key)
            && !options.readonly_fields.contains(&field.name)
            && !options.hidden_fields.contains(&field.name);
        if !writable {
            continue;
        }
        properties.insert(field.name.clone(), field_schema(field));
        if required(field) {
            needed.push(json!(field.name));
        }
    }
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        // Unknown, hidden and read-only fields are refused, not ignored.
        "additionalProperties": false,
    });
    if !needed.is_empty() {
        schema["required"] = Value::Array(needed);
    }
    for link in resource.links() {
        schema["properties"][&link.name] = link_schema(&link);
    }
    schema
}

/// A many-to-many field: the ids of the linked rows.
fn link_schema(link: &crate::resource::LinkField) -> Value {
    json!({
        "type": "array",
        "items": {},
        "description": format!("The ids of the linked `{}` rows", link.model),
    })
}

fn needs(resource: &dyn Resource, action: Action) -> String {
    format!(
        "Requires the `{}` permission on `{}`.",
        action.as_str(),
        resource.slug()
    )
}

/// Adds one model's schemas and paths.
fn describe_model(
    resource: &dyn Resource,
    api_base: &str,
    schemas: &mut Map<String, Value>,
    paths: &mut Map<String, Value>,
) {
    let slug = resource.slug();
    let name = schema_name(slug);
    let tag = resource.label();
    // `blog_post` -> `blog post`: one row, as opposed to the label, which names them all.
    let noun = slug.replace('_', " ");
    let article = if noun.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    let one = format!("{article} {noun}");
    let hidden = &resource.options().hidden_fields;

    let mut properties = Map::new();
    for field in resource
        .fields()
        .iter()
        .filter(|f| !hidden.contains(&f.name))
    {
        properties.insert(field.name.clone(), field_schema(field));
    }
    for link in resource.links() {
        properties.insert(link.name.clone(), link_schema(&link));
    }
    schemas.insert(
        name.clone(),
        json!({ "type": "object", "properties": properties }),
    );

    // `POST` may leave out what the database fills in; `PUT` replaces the whole record.
    // A UUID key left out is made up.
    let create = write_schema(resource, true, |f| {
        !f.nullable && !f.has_default && !(f.primary_key && f.kind == FieldKind::Uuid)
    });
    let replace = write_schema(resource, false, |f| !f.nullable);
    let (create_name, replace_name) = if create == replace {
        (format!("{name}Input"), format!("{name}Input"))
    } else {
        (format!("{name}Create"), format!("{name}Replace"))
    };
    schemas.insert(create_name.clone(), create);
    schemas.insert(replace_name.clone(), replace);
    let patch_name = format!("{name}Patch");
    schemas.insert(patch_name.clone(), write_schema(resource, false, |_| false));

    let sort_values: Vec<String> = resource
        .fields()
        .iter()
        .filter(|f| !hidden.contains(&f.name))
        .flat_map(|f| [f.name.clone(), format!("-{}", f.name)])
        .collect();
    // One parameter per column a list can be filtered by; the comparison suffixes are
    // described rather than listed, or every model would have hundreds of parameters.
    let reserved = ["page", "per_page", "q", "sort"];
    let mut parameters = vec![
        json!({ "name": "page", "in": "query", "description": "Page number, from 1", "schema": { "type": "integer", "minimum": 1, "default": 1 } }),
        json!({ "name": "per_page", "in": "query", "description": "Rows per page", "schema": { "type": "integer", "minimum": 1, "maximum": 100 } }),
        json!({ "name": "q", "in": "query", "description": "Search text; matches any searchable column, ignoring case", "schema": { "type": "string" } }),
        json!({ "name": "sort", "in": "query", "description": "Column to order by; prefix with `-` for highest first. By default rows come in primary-key order.", "schema": { "type": "string", "enum": sort_values } }),
    ];
    for field in resource.fields().iter().filter(|f| {
        !hidden.contains(&f.name)
            && !reserved.contains(&f.name.as_str())
            && !matches!(f.kind, FieldKind::Json | FieldKind::Other)
    }) {
        let mut description = format!(
            "Only rows whose {} equals this. Also `{name}__gt`, `{name}__gte`, `{name}__lt`, \
             `{name}__lte` to compare, and `{name}__isnull=true|false`",
            field.label.to_lowercase(),
            name = field.name
        );
        if matches!(field.kind, FieldKind::Date | FieldKind::DateTime) {
            description.push_str(&format!(
                "; `{}__range=today|7d|30d|month|year` for a period (UTC)",
                field.name
            ));
        }
        description.push('.');
        parameters.push(json!({
            "name": field.name,
            "in": "query",
            "description": description,
            "schema": { "type": "string" },
        }));
    }
    let list = json!({
        "type": "object",
        "properties": {
            "data": { "type": "array", "items": reference(&name) },
            "meta": reference("ListMeta"),
        },
    });
    paths.insert(
        format!("{api_base}/{slug}"),
        json!({
            "get": {
                "tags": [tag],
                "summary": format!("List {tag}"),
                "description": needs(resource, Action::View),
                "operationId": format!("list_{slug}"),
                "parameters": parameters,
                "responses": {
                    "200": json_response("One page of rows", &list),
                    "401": error_response("Unauthorized"),
                    "403": error_response("Forbidden"),
                    "422": error_response("Invalid"),
                },
            },
            "post": {
                "tags": [tag],
                "summary": format!("Create {one}"),
                "description": needs(resource, Action::Add),
                "operationId": format!("create_{slug}"),
                "requestBody": json_body(&reference(&create_name)),
                "responses": {
                    "201": {
                        "description": "The row as stored",
                        "headers": { "Location": { "description": "URL of the new row", "schema": { "type": "string" } } },
                        "content": { "application/json": { "schema": reference(&name) } },
                    },
                    "401": error_response("Unauthorized"),
                    "403": error_response("Forbidden"),
                    "409": error_response("Conflict"),
                    "422": error_response("Invalid"),
                },
            },
        }),
    );

    // Single rows are addressed by primary key; a model without exactly one has no such URLs.
    let mut keys = resource.fields().iter().filter(|f| f.primary_key);
    let (Some(key), None) = (keys.next(), keys.next()) else {
        return;
    };
    let key_type = if key.kind == FieldKind::Integer {
        "integer"
    } else {
        "string"
    };
    let write = |verb: &str, summary: &str, body: &str| {
        json!({
            "tags": [tag],
            "summary": summary,
            "description": needs(resource, Action::Change),
            "operationId": format!("{verb}_{slug}"),
            "requestBody": json_body(&reference(body)),
            "responses": {
                "200": json_response("The row as stored", &reference(&name)),
                "401": error_response("Unauthorized"),
                "403": error_response("Forbidden"),
                "404": error_response("NotFound"),
                "409": error_response("Conflict"),
                "422": error_response("Invalid"),
            },
        })
    };
    paths.insert(
        format!("{api_base}/{slug}/{{id}}"),
        json!({
            "parameters": [
                { "name": "id", "in": "path", "required": true, "description": format!("The row's `{}`", key.name), "schema": { "type": key_type } },
            ],
            "get": {
                "tags": [tag],
                "summary": format!("Get {one}"),
                "description": needs(resource, Action::View),
                "operationId": format!("get_{slug}"),
                "responses": {
                    "200": json_response("The row", &reference(&name)),
                    "401": error_response("Unauthorized"),
                    "403": error_response("Forbidden"),
                    "404": error_response("NotFound"),
                },
            },
            "put": write("replace", &format!("Replace {one}"), &replace_name),
            "patch": write("update", &format!("Change some fields of {one}"), &patch_name),
            "delete": {
                "tags": [tag],
                "summary": format!("Delete {one}"),
                "description": needs(resource, Action::Delete),
                "operationId": format!("delete_{slug}"),
                "responses": {
                    "204": { "description": "Deleted" },
                    "401": error_response("Unauthorized"),
                    "403": error_response("Forbidden"),
                    "404": error_response("NotFound"),
                    "409": error_response("Conflict"),
                },
            },
        }),
    );
}

/// Adds the file endpoints.
fn describe_files(
    api_base: &str,
    schemas: &mut Map<String, Value>,
    paths: &mut Map<String, Value>,
) {
    let tag = "Files";
    let needs = |action: Action| {
        format!(
            "Requires the `{}` permission on `{FILES}`.",
            action.as_str()
        )
    };
    schemas.insert(
        "File".to_owned(),
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "filename": { "type": "string", "description": "The name the client gave; the file is stored under a name of the server's own" },
                "content_type": { "type": "string" },
                "size": { "type": "integer", "description": "Bytes" },
                "created_by": { "type": "string" },
                "url": { "type": "string", "description": "Where to download it" },
            },
        }),
    );
    schemas.insert(
        "Upload".to_owned(),
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "filename": { "type": "string" },
                "size": { "type": "integer", "description": "Bytes the finished file will have" },
                "received": { "type": "integer", "description": "Bytes received so far: the offset of the next chunk" },
                "max_chunk_bytes": { "type": "integer" },
                "complete": { "type": "boolean" },
                "url": { "type": "string" },
            },
        }),
    );
    let file_id =
        json!({ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } });
    let upload_id =
        json!({ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } });
    let file_list = json!({
        "type": "object",
        "properties": {
            "data": { "type": "array", "items": reference("File") },
            "meta": reference("ListMeta"),
        },
    });
    paths.insert(
        format!("{api_base}/files"),
        json!({
            "get": {
                "tags": [tag], "summary": "List files", "description": needs(Action::View), "operationId": "list_files",
                "parameters": [{ "name": "page", "in": "query", "schema": { "type": "integer", "minimum": 1, "default": 1 } }],
                "responses": { "200": json_response("One page of files, newest first", &file_list), "401": error_response("Unauthorized"), "403": error_response("Forbidden") },
            },
            "post": {
                "tags": [tag], "summary": "Upload a file", "operationId": "upload_file",
                "description": format!("{} For large files or unreliable connections, use the resumable upload instead.", needs(Action::Add)),
                "requestBody": { "required": true, "content": { "multipart/form-data": { "schema": {
                    "type": "object", "required": ["file"],
                    "properties": { "file": { "type": "string", "format": "binary" } },
                } } } },
                "responses": { "201": json_response("The stored file", &reference("File")), "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "413": error_response("TooLarge") },
            },
        }),
    );
    paths.insert(
        format!("{api_base}/files/{{id}}"),
        json!({
            "parameters": [file_id],
            "get": {
                "tags": [tag], "summary": "Get a file's details", "description": needs(Action::View), "operationId": "get_file",
                "responses": { "200": json_response("The file", &reference("File")), "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound") },
            },
            "delete": {
                "tags": [tag], "summary": "Delete a file", "description": needs(Action::Delete), "operationId": "delete_file",
                "responses": { "204": { "description": "Deleted" }, "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound") },
            },
        }),
    );
    paths.insert(
        format!("{api_base}/files/{{id}}/content"),
        json!({
            "parameters": [file_id],
            "get": {
                "tags": [tag], "summary": "Download a file", "description": needs(Action::View), "operationId": "download_file",
                "responses": {
                    "200": { "description": "The file, always as an attachment", "content": { "application/octet-stream": { "schema": { "type": "string", "format": "binary" } } } },
                    "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound"),
                },
            },
        }),
    );
    paths.insert(
        format!("{api_base}/uploads"),
        json!({
            "post": {
                "tags": [tag], "summary": "Start a resumable upload", "operationId": "begin_upload",
                "description": format!("{} Then send the file in chunks with `PATCH`.", needs(Action::Add)),
                "requestBody": json_body(&json!({
                    "type": "object", "required": ["filename", "size"],
                    "properties": {
                        "filename": { "type": "string" },
                        "size": { "type": "integer", "description": "Size of the whole file in bytes" },
                        "content_type": { "type": "string" },
                    },
                })),
                "responses": { "201": json_response("The upload to send chunks to", &reference("Upload")), "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "413": error_response("TooLarge") },
            },
        }),
    );
    let offset = json!({ "Upload-Offset": { "description": "Bytes received so far", "schema": { "type": "integer" } } });
    paths.insert(
        format!("{api_base}/uploads/{{id}}"),
        json!({
            "parameters": [upload_id],
            "get": {
                "tags": [tag], "summary": "Get how much of an upload has arrived", "operationId": "upload_status",
                "description": format!("{} This is how a client resumes after a dropped connection.", needs(Action::Add)),
                "responses": { "200": { "description": "The upload", "headers": offset, "content": { "application/json": { "schema": reference("Upload") } } }, "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound") },
            },
            "patch": {
                "tags": [tag], "summary": "Send the next chunk of an upload", "operationId": "append_chunk",
                "description": format!("{} Chunks are sent in order. The chunk that completes the file answers `201` with the stored file.", needs(Action::Add)),
                "parameters": [{ "name": "Upload-Offset", "in": "header", "required": true, "description": "Position of this chunk in the file; must equal the bytes received so far", "schema": { "type": "integer" } }],
                "requestBody": { "required": true, "content": { "application/octet-stream": { "schema": { "type": "string", "format": "binary" } } } },
                "responses": {
                    "200": { "description": "Chunk stored; more are expected", "headers": offset, "content": { "application/json": { "schema": reference("Upload") } } },
                    "201": json_response("The file is complete", &reference("File")),
                    "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound"),
                    "409": error_response("Conflict"), "413": error_response("TooLarge"),
                },
            },
            "delete": {
                "tags": [tag], "summary": "Cancel an upload", "description": needs(Action::Add), "operationId": "abort_upload",
                "responses": { "204": { "description": "Cancelled" }, "401": error_response("Unauthorized"), "403": error_response("Forbidden"), "404": error_response("NotFound") },
            },
        }),
    );
}

fn error(description: &str) -> Value {
    json_response(description, &reference("Error"))
}

/// The logins for apps under `{api_base}/auth/`, as far as this site offers them.
fn describe_auth(
    api_base: &str,
    auth: &crate::auth::AuthConfig,
    accounts: bool,
    schemas: &mut Map<String, Value>,
    paths: &mut Map<String, Value>,
) {
    let methods = &auth.methods;
    let tag = "Authentication";
    let ok = || json_response("Logged in", &reference("Token"));
    let challenge = || {
        json_response(
            "The account has two-factor login: send the `challenge` with a code to `POST /auth/totp` (`totp_required`)",
            &reference("Challenge"),
        )
    };
    let object = |properties: Value, required: &[&str]| json!({ "type": "object", "required": required, "properties": properties });
    let public = |summary: &str, description: &str, id: &str, body: Value, responses: Value| {
        json!({ "post": {
            "tags": [tag], "summary": summary, "description": description, "operationId": id,
            "security": [], "requestBody": json_body(&body), "responses": responses,
        } })
    };
    let mut add = |path: &str, item: Value| {
        paths.insert(format!("{api_base}/auth/{path}"), item);
    };

    schemas.insert(
        "Challenge".to_owned(),
        json!({
            "type": "object",
            "properties": {
                "error": { "$ref": "#/components/schemas/Error/properties/error" },
                "challenge": { "type": "string", "description": "Send with a code to `POST /auth/totp` within 5 minutes; present only for `totp_required`" },
            },
        }),
    );

    add(
        "totp",
        public(
            "Finish a two-factor login",
            "The second step of any login for an account with two-factor login: the `challenge` it answered with, and a code from the authenticator app or a recovery code.",
            "login_totp",
            object(
                json!({ "challenge": { "type": "string" }, "code": { "type": "string", "example": "123456" } }),
                &["challenge", "code"],
            ),
            json!({ "200": ok(), "401": error("Wrong code (`unauthorized`) or expired challenge (`challenge_expired`)"), "429": error_response("RateLimited") }),
        ),
    );
    if accounts && auth.refresh_ttl_secs > 0 {
        let refresh = object(
            json!({ "refresh_token": { "type": "string" } }),
            &["refresh_token"],
        );
        add(
            "refresh",
            public(
                "Refresh a login",
                "A new access token and refresh token for a refresh token, which is then spent. Using a spent one again ends that whole login.",
                "refresh_token",
                refresh.clone(),
                json!({ "200": ok(), "401": error("Not valid, expired or already used (`invalid_refresh_token`)") }),
            ),
        );
        add(
            "logout",
            public(
                "Log out",
                "Ends the login the refresh token belongs to. The access token works until it expires.",
                "logout",
                refresh,
                json!({ "204": { "description": "Logged out" } }),
            ),
        );
    }

    if !methods.providers.is_empty() {
        let names: Vec<&str> = methods.providers.iter().map(|p| p.name.as_str()).collect();
        let id_tokens: Vec<&str> = methods
            .providers
            .iter()
            .filter(|p| p.issues_id_tokens())
            .map(|p| p.name.as_str())
            .collect();
        add(
            "{provider}",
            json!({ "post": {
                "tags": [tag],
                "summary": "Log in through another site",
                "description": format!(
                    "The app logged the person in with the provider and sends what it got: an `id_token` (providers {}), or the `code` with the `redirect_uri` and PKCE `code_verifier` it used (any provider). New addresses get an account if sign-up allows them.",
                    if id_tokens.is_empty() { "none here".to_owned() } else { id_tokens.join(", ") }
                ),
                "operationId": "login_provider",
                "security": [],
                "parameters": [{ "name": "provider", "in": "path", "required": true, "schema": { "type": "string", "enum": names } }],
                "requestBody": json_body(&json!({ "type": "object", "properties": {
                    "id_token": { "type": "string" },
                    "nonce": { "type": "string", "description": "The nonce the app sent to the provider, if any" },
                    "code": { "type": "string" },
                    "code_verifier": { "type": "string" },
                    "redirect_uri": { "type": "string", "description": "One of `auth.app.redirect_uris`" },
                } })),
                "responses": {
                    "200": ok(), "401": challenge(),
                    "403": error("No account for that address, and sign-up does not allow it (`no_account`)"),
                    "422": error_response("Invalid"),
                },
            } }),
        );
    }

    let mail = accounts && methods.app.url.is_some();
    let email = || {
        object(
            json!({ "email": { "type": "string", "format": "email" } }),
            &["email"],
        )
    };
    let sent = || json!({ "202": { "description": "If the address may do this, a message with a link to the app is on its way" }, "429": error_response("RateLimited") });
    if mail && methods.magic_link {
        add(
            "link",
            public(
                "Send a login link",
                "E-mails a login link to the app; the app sends its token to `POST /auth/link/verify`.",
                "login_link",
                email(),
                sent(),
            ),
        );
        add(
            "link/verify",
            public(
                "Log in with a link",
                "The token from a login link. It works once, within 15 minutes.",
                "login_link_verify",
                object(json!({ "token": { "type": "string" } }), &["token"]),
                json!({ "200": ok(), "401": challenge(), "400": error("Expired or used (`invalid_link`)") }),
            ),
        );
    }
    if mail && methods.password {
        add(
            "register",
            public(
                "Sign up",
                "E-mails a confirmation link to an address sign-up allows; the password is chosen at `POST /auth/register/verify`.",
                "register",
                email(),
                sent(),
            ),
        );
        add(
            "register/verify",
            public(
                "Confirm a sign-up",
                "The token from a confirmation link and the password to use. Creates the account and logs it in.",
                "register_verify",
                object(
                    json!({ "token": { "type": "string" }, "password": { "type": "string", "format": "password" } }),
                    &["token", "password"],
                ),
                json!({ "200": ok(), "400": error("Expired or used (`invalid_link`)"), "422": error_response("Invalid") }),
            ),
        );
        add(
            "password/forgot",
            public(
                "Forgot the password",
                "E-mails a link to the app for choosing a new password.",
                "password_forgot",
                email(),
                sent(),
            ),
        );
        add(
            "password/reset",
            public(
                "Choose a new password",
                "The token from a reset link and the new password. Every login made with the old password ends.",
                "password_reset",
                object(
                    json!({ "token": { "type": "string" }, "password": { "type": "string", "format": "password" } }),
                    &["token", "password"],
                ),
                json!({ "204": { "description": "Changed" }, "400": error("Expired or used (`invalid_link`)"), "422": error_response("Invalid") }),
            ),
        );
    }

    let mine = |summary: &str, description: &str, id: &str| {
        json!({ "tags": [tag], "summary": summary, "description": description, "operationId": id,
                "responses": { "401": error_response("Unauthorized"), "403": error_response("Forbidden") } })
    };
    let with_body = |mut operation: Value, body: Value, success: (&str, Value)| {
        operation["requestBody"] = json_body(&body);
        operation["responses"][success.0] = success.1;
        operation
    };
    let mut me = json!({ "get": mine("The logged-in account", "Who the request is logged in as: username, staff, superuser and, for the built-in accounts, the profile.", "me") });
    me["get"]["responses"]["200"] = json!({ "description": "The account" });
    if accounts {
        me["patch"] = with_body(
            mine(
                "Change your profile",
                "First and last name, phone. Empty values clear a field.",
                "me_update",
            ),
            object(
                json!({ "first_name": { "type": "string" }, "last_name": { "type": "string" }, "phone": { "type": "string" } }),
                &[],
            ),
            ("204", json!({ "description": "Changed" })),
        );
    }
    add("me", me);
    if !accounts {
        return;
    }
    if methods.password {
        add(
            "me/password",
            json!({ "post": with_body(
            mine("Change your password", "With the current password. Answers with new tokens: the change ends every older login.", "me_password"),
            object(json!({ "current": { "type": "string", "format": "password" }, "password": { "type": "string", "format": "password" } }), &["current", "password"]),
            ("200", ok()),
        ) }),
        );
    }
    let code = || object(json!({ "code": { "type": "string" } }), &["code"]);
    let mut setup = mine(
        "Start two-factor setup",
        "A new authenticator key (`secret`, and an `otpauth://` `uri` for a QR code), stored once confirmed.",
        "me_totp",
    );
    setup["responses"]["200"] = json!({ "description": "The key to scan" });
    add("me/totp", json!({ "post": setup }));
    add(
        "me/totp/confirm",
        json!({ "post": with_body(
        mine("Turn two-factor login on", "The key from `POST /auth/me/totp` and a current code from it; `password` unless the login is under 15 minutes old. Answers with the recovery codes, shown once.", "me_totp_confirm"),
        object(json!({ "secret": { "type": "string" }, "code": { "type": "string" }, "password": { "type": "string", "format": "password" } }), &["secret", "code"]),
        ("200", json!({ "description": "`recovery_codes`" })),
    ) }),
    );
    add(
        "me/totp/disable",
        json!({ "post": with_body(
        mine("Turn two-factor login off", "With a current code or a recovery code.", "me_totp_disable"),
        code(),
        ("204", json!({ "description": "Off" })),
    ) }),
    );
    add(
        "me/recovery-codes",
        json!({ "post": with_body(
        mine("New recovery codes", "Replaces the recovery codes, for a current code from the authenticator app.", "me_recovery_codes"),
        code(),
        ("200", json!({ "description": "`recovery_codes`" })),
    ) }),
    );
    let mut links = mine(
        "Your linked sites",
        "The other sites this account logs in through.",
        "me_identities",
    );
    links["responses"]["200"] = json!({ "description": "`data`: the links" });
    add("me/identities", json!({ "get": links }));
    let mut unlink = mine(
        "Forget a linked site",
        "The next login through it is matched by e-mail address again.",
        "me_identity_delete",
    );
    unlink["responses"]["204"] = json!({ "description": "Forgotten" });
    unlink["parameters"] =
        json!([{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }]);
    add("me/identities/{id}", json!({ "delete": unlink }));
    if auth.api_keys {
        let mut list = mine(
            "Your API keys",
            "The keys themselves are never shown again.",
            "me_api_keys",
        );
        list["responses"]["200"] = json!({ "description": "`data`: the keys" });
        add(
            "me/api-keys",
            json!({
                "get": list,
                "post": with_body(
                    mine("Make an API key", "Shown in this answer only. `expires_in_days` is 30, 90 (default), 365 or null for never; `password` unless the login is under 15 minutes old. API keys cannot make keys.", "me_api_key_create"),
                    object(json!({ "name": { "type": "string" }, "expires_in_days": { "type": "integer", "nullable": true }, "read_only": { "type": "boolean" }, "password": { "type": "string", "format": "password" } }), &["name"]),
                    ("201", json!({ "description": "`key`: the new key" })),
                ),
            }),
        );
        let mut revoke = mine("Revoke an API key", "", "me_api_key_revoke");
        revoke["responses"]["204"] = json!({ "description": "Revoked" });
        revoke["parameters"] = json!([{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }]);
        add("me/api-keys/{id}", json!({ "delete": revoke }));
    }
}

/// Builds the OpenAPI 3.0 document for everything the app generates.
fn document(
    config: &DocsConfig,
    title: &str,
    api_base: &str,
    resources: &[Arc<dyn Resource>],
    uploads: bool,
    auth: &crate::auth::AuthConfig,
    accounts: bool,
) -> Value {
    let api_keys = auth.api_keys;
    let mut schemas = Map::new();
    let mut paths = Map::new();
    let token_url = format!("{api_base}/auth/token");

    schemas.insert(
        "Error".to_owned(),
        json!({
            "type": "object",
            "properties": { "error": { "type": "object", "properties": {
                "code": { "type": "string", "description": "Stable, machine-readable", "example": "not_found" },
                "message": { "type": "string" },
                "field": { "type": "string", "nullable": true, "description": "The field at fault, for `invalid_field`" },
            } } },
        }),
    );
    schemas.insert(
        "ListMeta".to_owned(),
        json!({
            "type": "object",
            "properties": {
                "total": { "type": "integer", "description": "Rows matching the request, on all pages" },
                "page": { "type": "integer" },
                "per_page": { "type": "integer" },
            },
        }),
    );
    schemas.insert(
        "Credentials".to_owned(),
        json!({
            "type": "object", "required": ["username", "password"],
            "properties": {
                "username": { "type": "string", "description": "Username, or the account's e-mail address" },
                "password": { "type": "string", "format": "password" },
                "totp": { "type": "string", "description": "Six-digit authenticator code, for accounts with two-factor authentication", "example": "123456" },
            },
        }),
    );
    schemas.insert(
        "Token".to_owned(),
        json!({
            "type": "object",
            "properties": {
                "access_token": { "type": "string", "description": "Send as `Authorization: Bearer <token>`" },
                "token": { "type": "string", "description": "The same token, under its older name" },
                "token_type": { "type": "string", "example": "Bearer" },
                "expires_in": { "type": "integer", "description": "Seconds" },
                "refresh_token": { "type": "string", "description": "Trade it for a new pair at `POST /auth/refresh` when the access token expires; it works once" },
                "refresh_expires_in": { "type": "integer", "description": "Seconds the refresh token lasts unused" },
            },
        }),
    );
    paths.insert(
        token_url.clone(),
        json!({
            "post": {
                "tags": ["Authentication"],
                "summary": "Log in",
                "description": "Exchanges a username and password for a bearer token. Accepts JSON or a form.",
                "operationId": "create_token",
                "security": [],
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": reference("Credentials") },
                    "application/x-www-form-urlencoded": { "schema": reference("Credentials") },
                } },
                "responses": {
                    "200": json_response("A token", &reference("Token")),
                    "401": json_response(
                        "Wrong username, password or authenticator code (`unauthorized`), or a right password for an account with two-factor login and no `totp` (`totp_required`, with a `challenge`)",
                        &reference("Challenge"),
                    ),
                    "403": error("Two-factor login is required but the account has none (`totp_setup_required`)"),
                    "429": error_response("RateLimited"),
                },
            },
        }),
    );
    describe_auth(api_base, auth, accounts, &mut schemas, &mut paths);

    let mut tags = vec![json!({ "name": "Authentication" })];
    for resource in resources {
        describe_model(resource.as_ref(), api_base, &mut schemas, &mut paths);
        tags.push(json!({ "name": resource.label() }));
    }
    if uploads {
        describe_files(api_base, &mut schemas, &mut paths);
        tags.push(json!({ "name": "Files" }));
    }

    let mut description = config.description.clone().unwrap_or_default();
    if !description.is_empty() {
        description.push_str("\n\n");
    }
    let _ = write!(
        description,
        "Authenticate with a bearer token from `POST {token_url}`{}. What an account may do \
         depends on its permissions; a refused request answers `403`.",
        if api_keys {
            ", or with an API key made on the account's API keys page"
        } else {
            ""
        }
    );

    let mut document = json!({
        "openapi": "3.0.3",
        "info": { "title": title, "version": config.version, "description": description },
        "tags": tags,
        "paths": paths,
        // Either way of supplying the token will do.
        "security": [{ "password": [] }, { "bearer": [] }],
        "components": {
            "schemas": schemas,
            "securitySchemes": {
                "password": {
                    "type": "oauth2",
                    "description": "Log in with a username and password. Accounts with two-factor authentication use `bearer` instead.",
                    "flows": { "password": { "tokenUrl": token_url, "scopes": {} } },
                },
                "bearer": {
                    "type": "http", "scheme": "bearer", "bearerFormat": "JWT",
                    "description": format!("A token from `POST {token_url}`, or an API key (`cadmus_...`)"),
                },
            },
            "responses": {
                "Unauthorized": error("No valid token was sent (`unauthorized`)"),
                "Forbidden": error("The account lacks the permission (`forbidden`)"),
                "NotFound": error("No such row or model (`not_found`)"),
                "Conflict": error("The change conflicts with existing data, e.g. a unique or foreign key (`conflict`)"),
                "Invalid": error("A field is missing, unknown, read-only or of the wrong type (`invalid_field`)"),
                "TooLarge": error("The upload exceeds a size limit (`too_large`)"),
                "RateLimited": {
                    "description": "Too many attempts (`rate_limited`)",
                    "headers": { "Retry-After": { "description": "Seconds to wait", "schema": { "type": "integer" } } },
                    "content": { "application/json": { "schema": reference("Error") } },
                },
            },
        },
    });
    if api_keys {
        document["security"]
            .as_array_mut()
            .expect("security is a list")
            .push(json!({ "apiKey": [] }));
        document["components"]["securitySchemes"]["apiKey"] = json!({
            "type": "apiKey", "in": "header", "name": "X-API-Key",
            "description": "An API key from the account's API keys page in the admin. It can \
                            also be sent as a bearer token.",
        });
    }
    document
}
