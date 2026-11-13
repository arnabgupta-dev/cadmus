//! The framework entry point: one [`App`] owns the database, the registered models, the admin,
//! the REST API and the management commands.

use std::io::{BufRead, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, middleware};
use clap::{Parser, Subcommand};
use sea_orm::schema::SchemaBuilder;
use sea_orm::{
    ActiveModelBehavior, ConnectOptions, Database, DatabaseConnection, EntityTrait, IntoActiveModel,
};

use crate::auth::totp_uri;
use crate::auth::{AuthConfig, UserStore};
use crate::docs::DocsConfig;
use crate::error::AdminError;
use crate::home::{self, Home, HomeConfig};
use crate::resource::{ModelOptions, Resource};
use crate::seaorm::SeaOrmResource;
use crate::settings::UiSettings;
use crate::storage::Storage;
use crate::uploads::Uploads;
use crate::users::{
    DbUserStore, change_password, create_superuser, disable_totp, enable_totp, promote,
};
use crate::{Admin, base_path, server};

type SchemaStep = Box<dyn Fn(SchemaBuilder) -> SchemaBuilder + Send + Sync>;

/// An application: register models, add your own routes, then call [`App::run`].
///
/// ```ignore
/// #[tokio::main]
/// async fn main() -> Result<(), AdminError> {
///     App::from_env()
///         .await?
///         .model::<post::Entity>(ModelOptions::new().label("Posts"))
///         .routes(my_routes())
///         .run()
///         .await
/// }
/// ```
///
/// Each registered model gets its table (created or altered by `migrate`), its admin pages and
/// its REST endpoints.
pub struct App {
    db: DatabaseConnection,
    auth: AuthConfig,
    settings: UiSettings,
    bind: String,
    admin_base: Option<String>,
    api_base: Option<String>,
    users: Arc<dyn UserStore>,
    /// Whether `users` is still the built-in store, which the Users and Groups pages edit.
    builtin_users: bool,
    resources: Vec<Arc<dyn Resource>>,
    schema: Vec<SchemaStep>,
    uploads: Option<Uploads>,
    routes: Router,
    request_timeout: Option<Duration>,
    cors_origins: Vec<String>,
    docs: DocsConfig,
    home: HomeConfig,
}

impl App {
    /// Builds an app over an existing connection, with accounts in the built-in `auth_user` table.
    #[must_use]
    pub fn new(db: DatabaseConnection, auth: AuthConfig) -> Self {
        Self {
            users: Arc::new(DbUserStore::new(db.clone())),
            builtin_users: true,
            db,
            auth,
            settings: UiSettings::default(),
            bind: "127.0.0.1:3000".to_owned(),
            admin_base: None,
            api_base: None,
            resources: Vec::new(),
            schema: Vec::new(),
            uploads: None,
            routes: Router::new(),
            request_timeout: Some(Duration::from_secs(30)),
            cors_origins: Vec::new(),
            docs: DocsConfig::new(),
            home: HomeConfig::new(),
        }
    }

    /// Builds an app from the environment (a `.env` file in the working directory is loaded
    /// first, without overriding variables that are already set).
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `DATABASE_URL` | **Required.** e.g. `sqlite://db.sqlite3?mode=rwc`, `postgres://…` |
    /// | `APP_BIND` | Listen address (default `127.0.0.1:3000`) |
    /// | `ADMIN_BASE_PATH` | Admin URL prefix (default `/admin`) |
    /// | `API_BASE_PATH` | REST API URL prefix (default `/api`) |
    /// | `DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS` | Connection pool size |
    /// | `DB_ACQUIRE_TIMEOUT_SECS` | How long a request waits for a pooled connection |
    /// | `STORAGE_URL` | Enables file uploads; see [`Storage::from_url`] |
    /// | `UPLOAD_MAX_FILE_BYTES`, `UPLOAD_MAX_CHUNK_BYTES` | Upload limits |
    /// | `REQUEST_TIMEOUT_SECS` | Longest a request may take; `0` for no limit (default 30) |
    /// | `CORS_ALLOWED_ORIGINS` | Origins whose pages may call the app from a browser, comma-separated, or `*` (default: none) |
    ///
    /// plus everything read by [`AuthConfig::from_env`], [`UiSettings::from_env`] and
    /// [`DocsConfig::from_env`].
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] for missing or invalid variables and [`AdminError::Db`] if
    /// the database cannot be reached.
    pub async fn from_env() -> Result<Self, AdminError> {
        // A missing `.env` is normal in production, where the platform sets the variables.
        let _ = dotenvy::dotenv();
        let url = std::env::var("DATABASE_URL")
            .map_err(|_| AdminError::Config("DATABASE_URL is not set".into()))?;
        let db = Database::connect(pool_options(url)?).await?;
        let uploads = Uploads::from_env(db.clone())?;
        let mut app = Self::new(db, AuthConfig::from_env()?).settings(UiSettings::from_env()?);
        if let Ok(bind) = std::env::var("APP_BIND") {
            app.bind = bind;
        }
        app.admin_base = std::env::var("ADMIN_BASE_PATH").ok();
        app.api_base = std::env::var("API_BASE_PATH").ok();
        app.uploads = uploads;
        if let Ok(text) = std::env::var("REQUEST_TIMEOUT_SECS") {
            let secs = text.parse::<u64>().map_err(|_| {
                AdminError::Config("REQUEST_TIMEOUT_SECS must be a non-negative integer".into())
            })?;
            app = app.request_timeout((secs > 0).then(|| Duration::from_secs(secs)));
        }
        if let Ok(text) = std::env::var("CORS_ALLOWED_ORIGINS") {
            app.cors_origins = server::parse_origins(&text)?;
        }
        app.docs = DocsConfig::from_env()?;
        app.home = HomeConfig::from_env()?;
        Ok(app)
    }

    /// The database connection, for your own handlers.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Look-and-feel settings for the admin.
    #[must_use]
    pub fn settings(mut self, settings: UiSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Listen address for `serve`.
    #[must_use]
    pub fn bind(mut self, address: impl Into<String>) -> Self {
        self.bind = address.into();
        self
    }

    /// Enables file uploads on `storage` with default limits.
    #[must_use]
    pub fn storage(mut self, storage: Storage) -> Self {
        self.uploads = Some(Uploads::new(self.db.clone(), storage));
        self
    }

    /// Enables file uploads with a fully configured service.
    #[must_use]
    pub fn uploads(mut self, uploads: Uploads) -> Self {
        self.uploads = Some(uploads);
        self
    }

    /// Replaces the built-in account store. The Users and Groups pages are then left out,
    /// because they edit the built-in tables.
    #[must_use]
    pub fn users(mut self, users: impl UserStore + 'static) -> Self {
        self.users = Arc::new(users);
        self.builtin_users = false;
        self
    }

    /// Settings for the public page at `/`, which shows whether the service is up. On by
    /// default; it also sends people who open an API URL in a browser there.
    ///
    /// Your own `/` route takes precedence over it. If your routes set a fallback with
    /// `Router::fallback`, turn the home page off, since both answer unmatched requests.
    #[must_use]
    pub fn home(mut self, home: HomeConfig) -> Self {
        self.home = home;
        self
    }

    /// Settings for the API documentation served under the API path at `docs`, `redoc` and
    /// `openapi.json`. By default it is on and open to everyone.
    #[must_use]
    pub fn docs(mut self, docs: DocsConfig) -> Self {
        self.docs = docs;
        self
    }

    /// Sets how long a request may take before it is answered with `503` and abandoned;
    /// `None` for no limit. The default is 30 seconds.
    ///
    /// It applies to the admin, the API and your own routes, but not to file uploads and
    /// downloads, which take as long as the files are big. Only the time until the response
    /// starts is counted, so streamed responses are not cut short.
    #[must_use]
    pub fn request_timeout(mut self, limit: Option<Duration>) -> Self {
        self.request_timeout = limit;
        self
    }

    /// Lets pages served from these origins (e.g. `https://app.example.com`) call the app
    /// from a browser; `*` allows every origin. By default no cross-origin access is allowed.
    ///
    /// Browser clients send the bearer token; cookies are never accepted across origins.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if an entry is not an origin.
    pub fn cors_origins<I, S>(mut self, origins: I) -> Result<Self, AdminError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut all = Vec::new();
        for origin in origins {
            all.extend(server::parse_origins(origin.as_ref())?);
        }
        self.cors_origins = all;
        Ok(self)
    }

    /// Registers a SeaORM entity: its table is managed by `migrate`, and it appears in the
    /// admin and the REST API.
    #[must_use]
    pub fn model<E>(mut self, options: ModelOptions) -> Self
    where
        E: EntityTrait,
        E::Model: IntoActiveModel<E::ActiveModel>,
        E::ActiveModel: ActiveModelBehavior + Send,
    {
        let resource = SeaOrmResource::<E>::new(self.db.clone()).options(options);
        self.resources.push(Arc::new(resource));
        self.schema
            .push(Box::new(|builder| builder.register(E::default())));
        self
    }

    /// Registers a resource that is not a SeaORM entity (admin and API only; no table).
    #[must_use]
    pub fn resource(mut self, resource: impl Resource + 'static) -> Self {
        self.resources.push(Arc::new(resource));
        self
    }

    /// Adds your own axum routes next to the generated ones.
    #[must_use]
    pub fn routes(mut self, routes: Router) -> Self {
        self.routes = self.routes.merge(routes);
        self
    }

    /// Creates missing tables and columns for the built-in accounts and every registered model.
    ///
    /// # Errors
    /// Returns [`AdminError::Db`] if the schema cannot be read or changed.
    pub async fn migrate(&self) -> Result<(), AdminError> {
        let mut builder = crate::users::register_schema(self.db.get_schema_builder());
        if self.uploads.is_some() {
            builder = Uploads::register_schema(builder);
        }
        for step in &self.schema {
            builder = step(builder);
        }
        builder.sync(&self.db).await?;
        Ok(())
    }

    /// The OpenAPI description of the REST API as JSON, as served at `openapi.json`. Use it
    /// to generate clients without starting the server.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the configuration is inconsistent, or if the
    /// documentation is turned off.
    pub fn openapi(&self) -> Result<serde_json::Value, AdminError> {
        self.admin()?
            .openapi()
            .cloned()
            .ok_or_else(|| AdminError::Config("the API documentation is turned off".into()))
    }

    /// The admin and API as configured so far.
    fn admin(&self) -> Result<Admin, AdminError> {
        let mut builder = Admin::builder()
            .settings(self.settings.clone())
            .docs(self.docs.clone())
            .auth_shared(self.auth.clone(), Arc::clone(&self.users));
        if let Some(base) = &self.admin_base {
            builder = builder.base_path(base.clone());
        }
        if let Some(base) = &self.api_base {
            builder = builder.api_base_path(base.clone());
        }
        if let Some(uploads) = &self.uploads {
            builder = builder.uploads(uploads.clone());
        }
        if self.builtin_users {
            builder = builder.accounts(self.db.clone());
        }
        for resource in &self.resources {
            builder = builder.register_shared(Arc::clone(resource));
        }
        builder.build()
    }

    /// The complete router: admin, REST API and your own routes.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the configuration is inconsistent (see
    /// [`crate::AdminBuilder::build`]).
    pub fn router(&self) -> Result<Router, AdminError> {
        let admin = self.admin()?;

        // From the inside out. The time limit covers everything except file transfers.
        let mut router = admin.page_routes();
        if self.home.is_enabled() {
            let home = Arc::new(Home::new(
                self.home.clone(),
                Arc::clone(&admin.state),
                self.db.clone(),
            ));
            // A fallback rather than a route, so that a `/` route of the application wins.
            router = router.fallback(move |request| home::fallback(Arc::clone(&home), request));
        }
        let mut router = router.merge(self.routes.clone());
        if let Some(limit) = self.request_timeout {
            router = router.layer(middleware::from_fn_with_state(limit, server::timeout));
        }
        if let Some(uploads) = admin.upload_routes() {
            router = router.merge(uploads);
        }
        // Errors, including the time limit's, are put into the format of their area.
        router = admin.handle_errors(router);
        // Outside that, so that error responses carry the cross-origin headers too and
        // preflight requests are answered before anything asks for a login.
        if !self.cors_origins.is_empty() {
            router = router.layer(server::cors(&self.cors_origins));
        }
        if self.home.is_enabled() {
            let api = base_path(self.api_base.clone(), "/api")?;
            let pages = if self.docs.is_enabled() {
                ["docs", "redoc", "openapi.json"]
                    .iter()
                    .map(|page| format!("{api}/{page}"))
                    .collect()
            } else {
                Vec::new()
            };
            let rules = Arc::new(server::BrowserRedirect {
                api_base: api,
                pages,
            });
            router = router.layer(middleware::from_fn_with_state(
                rules,
                server::browser_redirect,
            ));
        }
        // Outermost, to log the response that is actually sent.
        Ok(router.layer(middleware::from_fn(server::access_log)))
    }

    /// Serves the app until Ctrl-C.
    ///
    /// # Errors
    /// Returns [`AdminError::Io`] if the address cannot be bound or the server fails.
    pub async fn serve(self) -> Result<(), AdminError> {
        let router = self.router()?;
        let listener = tokio::net::TcpListener::bind(&self.bind).await?;
        println!("listening on http://{}", listener.local_addr()?);
        // Connection addresses are what the login limiter counts attempts by.
        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        axum::serve(listener, service)
            .with_graceful_shutdown(async {
                // If the signal handler cannot be installed, keep serving rather than exit.
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            })
            .await?;
        Ok(())
    }

    /// Runs the management command given on the command line
    /// (`serve`, `migrate`, `createsuperuser`, `changepassword`, `promote`, `totp`, `openapi`,
    /// `routes`, `check`).
    ///
    /// # Errors
    /// Returns whatever the chosen command fails with.
    pub async fn run(self) -> Result<(), AdminError> {
        let command = Cli::parse().command;
        init_logging()?;
        match command {
            Command::Serve { bind } => match bind {
                Some(address) => self.bind(address).serve().await,
                None => self.serve().await,
            },
            Command::Migrate => {
                self.migrate().await?;
                println!("schema is up to date");
                Ok(())
            }
            Command::Createsuperuser {
                username,
                password_stdin,
            } => {
                let password = read_password(password_stdin)?;
                create_superuser(&self.db, &username, &password).await?;
                println!("created superuser `{username}`");
                Ok(())
            }
            Command::Changepassword {
                username,
                password_stdin,
            } => {
                let password = read_password(password_stdin)?;
                change_password(&self.db, &username, &password).await?;
                println!("password changed for `{username}`");
                Ok(())
            }
            Command::Totp { username, disable } => {
                if disable {
                    disable_totp(&self.db, &username).await?;
                    println!("two-factor authentication is off for `{username}`");
                    return Ok(());
                }
                let secret = enable_totp(&self.db, &username).await?;
                let uri = totp_uri(&self.settings.site_title, &username, &secret);
                if let Ok(code) = qrcode::QrCode::new(&uri) {
                    let picture = code
                        .render::<qrcode::render::unicode::Dense1x2>()
                        .quiet_zone(true)
                        .build();
                    println!("{picture}");
                }
                println!("two-factor authentication is on for `{username}`");
                println!(
                    "scan the code above with an authenticator app, or enter the key by hand:"
                );
                println!("  key: {secret}");
                println!("  uri: {uri}");
                Ok(())
            }
            Command::Promote { username } => {
                promote(&self.db, &username).await?;
                println!("`{username}` is now a superuser");
                Ok(())
            }
            Command::Openapi => {
                let document = self.openapi()?;
                let text = serde_json::to_string_pretty(&document)
                    .map_err(|e| AdminError::Config(format!("cannot print the document: {e}")))?;
                println!("{text}");
                Ok(())
            }
            Command::Routes => {
                drop(self.router()?);
                let admin = base_path(self.admin_base.clone(), "/admin")?;
                let api = base_path(self.api_base.clone(), "/api")?;
                if self.home.is_enabled() {
                    println!("GET    /");
                }
                println!("POST   {api}/auth/token");
                if self.docs.is_enabled() {
                    println!("GET    {api}/docs");
                    println!("GET    {api}/redoc");
                    println!("GET    {api}/openapi.json");
                }
                for resource in &self.resources {
                    let slug = resource.slug();
                    println!("GET    {admin}/{slug}");
                    println!("GET    {api}/{slug}");
                    println!("POST   {api}/{slug}");
                    println!("GET    {api}/{slug}/{{id}}");
                    println!("PUT    {api}/{slug}/{{id}}");
                    println!("PATCH  {api}/{slug}/{{id}}");
                    println!("DELETE {api}/{slug}/{{id}}");
                }
                Ok(())
            }
            Command::Check => {
                drop(self.router()?);
                println!("configuration is valid ({} models)", self.resources.len());
                Ok(())
            }
        }
    }
}

/// Prints the framework's log messages (failed requests, panics, rejected logins) to standard
/// error. Does nothing if the application has already installed a `tracing` subscriber.
///
/// `LOG_LEVEL` is `error`, `warn`, `info` (default), `debug` or `trace`. SQL statements are
/// logged from `debug` up.
fn init_logging() -> Result<(), AdminError> {
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let level = match std::env::var("LOG_LEVEL") {
        Ok(text) => text.parse::<Level>().map_err(|_| {
            AdminError::Config("LOG_LEVEL must be error, warn, info, debug or trace".into())
        })?,
        Err(_) => Level::INFO,
    };
    let sql = if level >= Level::DEBUG {
        level
    } else {
        Level::WARN
    };
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(level)
        .with_target("sqlx", sql);
    // Fails only when a subscriber is already installed, which is the application's choice.
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                // Colour only when a person is looking, not in a log file.
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .with(filter)
        .try_init();
    Ok(())
}

/// Connection-pool settings from the environment.
fn pool_options(url: String) -> Result<ConnectOptions, AdminError> {
    let number = |key: &str| -> Result<Option<u64>, AdminError> {
        match std::env::var(key) {
            Ok(text) => text
                .parse::<u64>()
                .map(Some)
                .map_err(|_| AdminError::Config(format!("{key} must be a non-negative integer"))),
            Err(_) => Ok(None),
        }
    };
    let mut options = ConnectOptions::new(url);
    if let Some(max) = number("DB_MAX_CONNECTIONS")? {
        options.max_connections(u32::try_from(max).unwrap_or(u32::MAX));
    }
    if let Some(min) = number("DB_MIN_CONNECTIONS")? {
        options.min_connections(u32::try_from(min).unwrap_or(u32::MAX));
    }
    if let Some(secs) = number("DB_ACQUIRE_TIMEOUT_SECS")? {
        options.acquire_timeout(std::time::Duration::from_secs(secs));
    }
    Ok(options)
}

#[derive(Parser)]
#[command(about = "Management commands", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    Serve {
        /// Listen address, overriding APP_BIND.
        #[arg(long)]
        bind: Option<String>,
    },
    /// Create or update tables for the built-in accounts and all registered models.
    Migrate,
    /// Create a superuser: an account that can do everything, including managing accounts.
    Createsuperuser {
        /// Login name.
        #[arg(long)]
        username: String,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change an account's password.
    Changepassword {
        /// Login name.
        #[arg(long)]
        username: String,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Turn two-factor authentication on for an account and print the code to scan.
    Totp {
        /// Login name.
        #[arg(long)]
        username: String,
        /// Turn it off instead, e.g. after a device was lost.
        #[arg(long)]
        disable: bool,
    },
    /// Make an existing account an active superuser.
    Promote {
        /// Login name.
        #[arg(long)]
        username: String,
    },
    /// Print the OpenAPI description of the REST API as JSON.
    Openapi,
    /// List the generated URLs.
    Routes,
    /// Validate the configuration without starting the server.
    Check,
}

fn read_password(from_stdin: bool) -> Result<String, AdminError> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_owned());
    }
    let first = rpassword::prompt_password("Password: ")?;
    let second = rpassword::prompt_password("Password (again): ")?;
    if first != second {
        return Err(AdminError::BadInput {
            field: "password".into(),
            message: "the two passwords do not match".into(),
        });
    }
    Ok(first)
}
