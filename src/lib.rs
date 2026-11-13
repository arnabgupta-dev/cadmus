//! A Django-admin-style panel for axum, generated from your SeaORM entities.
//!
//! Register entities and the crate serves list, search, create, edit and delete pages for
//! each one, behind password + TOTP login with JWT sessions. Look and feel
//! ([`UiSettings`]) and authentication ([`AuthConfig`], [`EnvUserStore`]) can be configured
//! entirely from environment variables.
//!
//! ```ignore
//! let admin = Admin::builder()
//!     .settings(UiSettings::from_env()?)
//!     .auth(AuthConfig::from_env()?, EnvUserStore::from_env()?)
//!     .register(SeaOrmResource::<post::Entity>::new(db.clone()))
//!     .build()?;
//! let app = axum::Router::new().merge(admin.router());
//! ```

mod access;
mod accounts;
mod api;
mod app;
mod auth;
mod error;
mod handlers;
mod resource;
mod seaorm;
mod settings;
mod storage;
mod uploads;
mod users;

use std::sync::Arc;

pub use access::{Access, Action};
pub use app::App;
pub use auth::{
    AdminUser, AuthConfig, EnvUserStore, UserStore, hash_password, hash_password_async, verify_totp,
};
pub use error::{AdminError, ApiError, ApiResult};
pub use resource::{
    Field, FieldKind, Input, ListQuery, ModelOptions, Page, Resource, Row, WriteMode,
};
pub use seaorm::SeaOrmResource;
pub use settings::{NavLink, Theme, UiSettings};
pub use storage::{Sink, Storage};
pub use uploads::{Progress, Uploads, file, session};
pub use users::{DbUserStore, change_password, create_superuser, group, promote, user};

/// Template names and their built-in sources.
const TEMPLATES: [(&str, &str); 9] = [
    ("base.html", include_str!("../templates/base.html")),
    ("login.html", include_str!("../templates/login.html")),
    ("index.html", include_str!("../templates/index.html")),
    ("list.html", include_str!("../templates/list.html")),
    ("form.html", include_str!("../templates/form.html")),
    ("error.html", include_str!("../templates/error.html")),
    (
        "accounts_list.html",
        include_str!("../templates/accounts_list.html"),
    ),
    (
        "user_form.html",
        include_str!("../templates/user_form.html"),
    ),
    (
        "group_form.html",
        include_str!("../templates/group_form.html"),
    ),
];

/// URL segments the admin uses itself; a model cannot take these as its slug.
const RESERVED_SLUGS: [&str; 6] = ["api", "auth", "files", "login", "logout", "uploads"];

pub(crate) struct AdminState {
    pub(crate) base: String,
    pub(crate) api_base: String,
    pub(crate) settings: UiSettings,
    pub(crate) auth: AuthConfig,
    pub(crate) users: Arc<dyn UserStore>,
    pub(crate) resources: Vec<Arc<dyn Resource>>,
    pub(crate) uploads: Option<Uploads>,
    /// Where the built-in account tables live; `None` when accounts come from elsewhere.
    pub(crate) accounts: Option<sea_orm::DatabaseConnection>,
    pub(crate) templates: minijinja::Environment<'static>,
}

impl AdminState {
    pub(crate) fn resource(&self, slug: &str) -> Result<&Arc<dyn Resource>, AdminError> {
        self.resources
            .iter()
            .find(|r| r.slug() == slug)
            .ok_or_else(|| AdminError::NotFound(format!("model `{slug}`")))
    }
}

/// A configured admin panel. Create with [`Admin::builder`], mount with [`Admin::router`].
#[derive(Clone)]
pub struct Admin {
    state: Arc<AdminState>,
}

impl Admin {
    /// Starts building an admin.
    #[must_use]
    pub fn builder() -> AdminBuilder {
        AdminBuilder::default()
    }

    /// Routes for the panel, already prefixed with the base path; `merge` it into your app.
    ///
    /// Errors from these routes are JSON under the API path and pages under the admin path.
    /// To get the same for routes of your own, and to turn panics into `500`s, pass the
    /// merged router through [`Admin::handle_errors`].
    pub fn router(&self) -> axum::Router {
        self.handle_errors(self.routes())
    }

    /// Adds the central error handling to `router`: panics become `500`s, and error
    /// responses (including unknown URLs and wrong methods) are JSON under the API path and
    /// pages under the admin path. Responses outside both paths are left alone.
    pub fn handle_errors(&self, router: axum::Router) -> axum::Router {
        router.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&self.state),
            error::middleware,
        ))
    }

    pub(crate) fn routes(&self) -> axum::Router {
        let mut router =
            handlers::router(Arc::clone(&self.state)).merge(api::router(Arc::clone(&self.state)));
        if self.state.accounts.is_some() {
            router = router.merge(accounts::router(Arc::clone(&self.state)));
        }
        match &self.state.uploads {
            Some(service) => router.merge(uploads::router(Arc::clone(&self.state), service)),
            None => router,
        }
    }
}

/// Builder for [`Admin`].
#[derive(Default)]
pub struct AdminBuilder {
    base: Option<String>,
    api_base: Option<String>,
    uploads: Option<Uploads>,
    accounts: Option<sea_orm::DatabaseConnection>,
    settings: UiSettings,
    auth: Option<(AuthConfig, Arc<dyn UserStore>)>,
    resources: Vec<Arc<dyn Resource>>,
}

impl AdminBuilder {
    /// URL prefix for the panel (default `/admin`).
    #[must_use]
    pub fn base_path(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into());
        self
    }

    /// Enables file uploads and downloads under the API base path.
    #[must_use]
    pub fn uploads(mut self, uploads: Uploads) -> Self {
        self.uploads = Some(uploads);
        self
    }

    /// Adds the Users and Groups pages, for superusers to manage the built-in accounts in
    /// `db`. Use it together with [`DbUserStore`] over the same database.
    #[must_use]
    pub fn accounts(mut self, db: sea_orm::DatabaseConnection) -> Self {
        self.accounts = Some(db);
        self
    }

    /// URL prefix for the REST API (default `/api`).
    #[must_use]
    pub fn api_base_path(mut self, base: impl Into<String>) -> Self {
        self.api_base = Some(base.into());
        self
    }

    pub(crate) fn register_shared(mut self, resource: Arc<dyn Resource>) -> Self {
        self.resources.push(resource);
        self
    }

    pub(crate) fn auth_shared(mut self, config: AuthConfig, users: Arc<dyn UserStore>) -> Self {
        self.auth = Some((config, users));
        self
    }

    /// Look-and-feel settings.
    #[must_use]
    pub fn settings(mut self, settings: UiSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Authentication settings and the account store. Required: there is no unauthenticated mode.
    #[must_use]
    pub fn auth(mut self, config: AuthConfig, users: impl UserStore + 'static) -> Self {
        self.auth = Some((config, Arc::new(users)));
        self
    }

    /// Adds a model to the panel.
    #[must_use]
    pub fn register(mut self, resource: impl Resource + 'static) -> Self {
        self.resources.push(Arc::new(resource));
        self
    }

    /// Validates the configuration and compiles the templates.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if auth is missing, the base path is malformed, a model
    /// slug is reserved or duplicated, or a template override cannot be read; and
    /// [`AdminError::Template`] if a template does not parse.
    pub fn build(self) -> Result<Admin, AdminError> {
        let (auth, users) = self.auth.ok_or_else(|| {
            AdminError::Config(
                "call .auth(...): the admin cannot be served without authentication".into(),
            )
        })?;

        let base = base_path(self.base, "/admin")?;
        let api_base = base_path(self.api_base, "/api")?;
        if base == api_base {
            return Err(AdminError::Config(
                "the admin and API base paths must differ".into(),
            ));
        }

        for (i, resource) in self.resources.iter().enumerate() {
            let slug = resource.slug();
            if RESERVED_SLUGS.contains(&slug) {
                return Err(AdminError::Config(format!(
                    "model slug `{slug}` is reserved"
                )));
            }
            if self.resources[..i].iter().any(|r| r.slug() == slug) {
                return Err(AdminError::Config(format!(
                    "model slug `{slug}` is registered twice"
                )));
            }
        }

        let mut templates = minijinja::Environment::new();
        for (name, builtin) in TEMPLATES {
            let source = match &self.settings.template_dir {
                Some(dir) if dir.join(name).is_file() => std::fs::read_to_string(dir.join(name))
                    .map_err(|e| {
                        AdminError::Config(format!("cannot read template override {name}: {e}"))
                    })?,
                _ => builtin.to_owned(),
            };
            templates.add_template_owned(name, source)?;
        }

        Ok(Admin {
            state: Arc::new(AdminState {
                base,
                api_base,
                settings: self.settings,
                auth,
                users,
                resources: self.resources,
                uploads: self.uploads,
                accounts: self.accounts,
                templates,
            }),
        })
    }
}

/// Normalises and validates a URL prefix.
fn base_path(configured: Option<String>, default: &str) -> Result<String, AdminError> {
    let base = configured.unwrap_or_else(|| default.to_owned());
    let base = base.trim_end_matches('/').to_owned();
    let valid = base.starts_with('/')
        && base.len() > 1
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'));
    if !valid {
        return Err(AdminError::Config(format!(
            "base path must look like {default} (letters, digits, `-`, `_`, `/`), got `{base}`"
        )));
    }
    Ok(base)
}
