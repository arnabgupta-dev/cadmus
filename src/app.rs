//! The framework entry point: one [`App`] owns the database, the registered models, the admin,
//! the REST API and the management commands.

use std::io::BufRead;
use std::sync::Arc;

use axum::Router;
use clap::{Parser, Subcommand};
use sea_orm::schema::SchemaBuilder;
use sea_orm::{
    ActiveModelBehavior, ConnectOptions, Database, DatabaseConnection, EntityTrait, IntoActiveModel,
};

use crate::auth::{AuthConfig, UserStore};
use crate::error::AdminError;
use crate::resource::{ModelOptions, Resource};
use crate::seaorm::SeaOrmResource;
use crate::settings::UiSettings;
use crate::storage::Storage;
use crate::uploads::Uploads;
use crate::users::{DbUserStore, change_password, create_superuser, user};
use crate::{Admin, base_path};

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
    resources: Vec<Arc<dyn Resource>>,
    schema: Vec<SchemaStep>,
    uploads: Option<Uploads>,
    routes: Router,
}

impl App {
    /// Builds an app over an existing connection, with accounts in the built-in `auth_user` table.
    #[must_use]
    pub fn new(db: DatabaseConnection, auth: AuthConfig) -> Self {
        Self {
            users: Arc::new(DbUserStore::new(db.clone())),
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
    ///
    /// plus everything read by [`AuthConfig::from_env`] and [`UiSettings::from_env`].
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

    /// Replaces the built-in account store.
    #[must_use]
    pub fn users(mut self, users: impl UserStore + 'static) -> Self {
        self.users = Arc::new(users);
        self
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
        let mut builder = self.db.get_schema_builder().register(user::Entity);
        if self.uploads.is_some() {
            builder = Uploads::register_schema(builder);
        }
        for step in &self.schema {
            builder = step(builder);
        }
        builder.sync(&self.db).await?;
        Ok(())
    }

    /// The complete router: admin, REST API and your own routes.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the configuration is inconsistent (see
    /// [`crate::AdminBuilder::build`]).
    pub fn router(&self) -> Result<Router, AdminError> {
        let mut builder = Admin::builder()
            .settings(self.settings.clone())
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
        for resource in &self.resources {
            builder = builder.register_shared(Arc::clone(resource));
        }
        Ok(builder.build()?.router().merge(self.routes.clone()))
    }

    /// Serves the app until Ctrl-C.
    ///
    /// # Errors
    /// Returns [`AdminError::Io`] if the address cannot be bound or the server fails.
    pub async fn serve(self) -> Result<(), AdminError> {
        let router = self.router()?;
        let listener = tokio::net::TcpListener::bind(&self.bind).await?;
        println!("listening on http://{}", listener.local_addr()?);
        axum::serve(listener, router)
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
    /// (`serve`, `migrate`, `createsuperuser`, `changepassword`, `routes`, `check`).
    ///
    /// # Errors
    /// Returns whatever the chosen command fails with.
    pub async fn run(self) -> Result<(), AdminError> {
        match Cli::parse().command {
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
            Command::Routes => {
                drop(self.router()?);
                let admin = base_path(self.admin_base.clone(), "/admin")?;
                let api = base_path(self.api_base.clone(), "/api")?;
                println!("POST   {api}/auth/token");
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
    /// Create an account that can use the admin and the API.
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
