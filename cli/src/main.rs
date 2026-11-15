//! `cadmus`: creates projects and models for cadmus.
//!
//! ```text
//! cadmus new blog
//! cd blog
//! cadmus generate model Post title:string body:text published:bool views:int rating:float?
//! cargo run -- makemigrations
//! cargo run -- migrate
//! cargo run -- createsuperuser --username admin
//! cargo run -- serve
//! ```

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod docs;
mod mcp;

/// The framework version generated projects ask for: this generator's own `major.minor`, as
/// the two are released together.
fn framework_version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    version
        .rsplit_once('.')
        .map_or(version, |(major_minor, _)| major_minor)
        .to_owned()
}
const MODULES_MARKER: &str = "// cadmus:modules (generated modules are added above this line)";
const MODELS_MARKER: &str = "// cadmus:models (generated models are registered above this line)";

#[derive(Parser)]
#[command(
    name = "cadmus",
    about = "Project and model generator for cadmus",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new project directory.
    New {
        /// Project (and crate) name: lowercase letters, digits, `-` and `_`.
        name: String,
        /// Use a local checkout of the framework instead of the version on crates.io.
        #[arg(long)]
        framework_path: Option<PathBuf>,
        /// Database the project starts with; only the address in .env depends on it, and
        /// changing `DATABASE_URL` switches to another one later.
        #[arg(long, value_enum, default_value_t = Database::Sqlite)]
        database: Database,
    },
    /// Generate code inside an existing project.
    #[command(alias = "g")]
    Generate {
        #[command(subcommand)]
        what: Generate,
    },
    /// Remove generated code from an existing project.
    #[command(alias = "destroy")]
    Delete {
        #[command(subcommand)]
        what: Delete,
    },
    /// Print a random value suitable for SECRET_KEY.
    Secret,
    /// Serve the Model Context Protocol over standard input and output, so AI coding agents
    /// can read the documentation and create projects, models and migrations.
    Mcp,
}

/// A database the framework has a driver for.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Database {
    Sqlite,
    Postgres,
    Mysql,
}

impl Database {
    /// A `DATABASE_URL` to start from.
    fn url(self, project: &str) -> String {
        let name = project.replace('-', "_");
        match self {
            Self::Sqlite => "sqlite://db.sqlite3?mode=rwc".to_owned(),
            Self::Postgres => format!("postgres://postgres:postgres@localhost:5432/{name}"),
            Self::Mysql => format!("mysql://root:root@localhost:3306/{name}"),
        }
    }
}

#[derive(Subcommand)]
enum Generate {
    /// Create a model with table, admin pages and REST endpoints.
    Model {
        /// Model name in PascalCase, e.g. `BlogPost`.
        name: String,
        /// Fields as `name:type`, with `?` for nullable. Types: string, text, int, bigint,
        /// float, bool, datetime, date, json, decimal, uuid, html (formatted text, edited with
        /// a rich-text editor), file, `references` for a foreign
        /// key to another model (`author:references`, or `editor:references=author`), `many`
        /// for a many-to-many field (`tags:many`, or `readers:many=user`), and `owner` for the
        /// account a row belongs to.
        fields: Vec<String>,
    },
}

#[derive(Subcommand)]
enum Delete {
    /// Remove a model: its file and its two lines in src/models/mod.rs. The next
    /// makemigrations then drops its table.
    Model {
        /// Model name in PascalCase, as it was generated, e.g. `BlogPost`.
        name: String,
        /// Delete without asking first.
        #[arg(long, short)]
        yes: bool,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::New {
            name,
            framework_path,
            database,
        } => new_project(Path::new("."), &name, framework_path.as_deref(), database),
        Command::Generate {
            what: Generate::Model { name, fields },
        } => generate_model(Path::new("."), &name, &fields),
        Command::Delete {
            what: Delete::Model { name, yes },
        } => (|| {
            let plan = plan_model_deletion(Path::new("."), &name)?;
            if !yes {
                if !std::io::stdin().is_terminal() {
                    return Err("add --yes to delete without being asked".into());
                }
                let question = format!(
                    "Delete the model {name} ({})? The next makemigrations drops its table `{}` \
                     and every row in it.",
                    plan.file.display(),
                    plan.module
                );
                if !confirm(&question) {
                    return Ok("nothing was deleted".into());
                }
            }
            delete_model(&plan)
        })(),
        Command::Secret => Ok(secret()),
        Command::Mcp => mcp::serve().map(|()| String::new()),
    };
    match result {
        Ok(report) => {
            if !report.is_empty() {
                println!("{report}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn secret() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn write_new(path: &Path, content: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))
}

fn new_project(
    parent: &Path,
    name: &str,
    framework_path: Option<&Path>,
    database: Database,
) -> Result<String, String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
    if !valid {
        return Err(
            "project names start with a lowercase letter and use lowercase letters, digits, `-`, `_`"
                .into(),
        );
    }
    let root = parent.join(name);
    if root.exists() {
        return Err(format!("{} already exists", root.display()));
    }
    // Every driver is built in by default, so the database is chosen in settings.
    let framework = match framework_path {
        Some(path) => {
            let absolute = path
                .canonicalize()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            format!("{{ path = {:?} }}", absolute.display().to_string())
        }
        None => format!("{:?}", framework_version()),
    };

    write_new(
        &root.join("Cargo.toml"),
        &format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
axum = "0.8"
cadmus = {framework}
sea-orm = {{ version = "2", default-features = false, features = ["macros", "with-json", "with-chrono"] }}
tokio = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
"#
        ),
    )?;
    write_new(
        &root.join("src/main.rs"),
        r"mod migrations;
mod models;
mod routes;
mod settings;

use cadmus::{AdminError, App};

#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_settings(settings::settings()?)
        .await?
        .migrations(migrations::all());
    models::register(app).routes(routes::router()).run().await
}
",
    )?;
    write_new(&root.join("src/settings.rs"), &settings_source(name))?;
    write_new(
        &root.join("src/migrations/mod.rs"),
        r"//! The project's database migrations, oldest first.
//!
//! `cargo run -- makemigrations` adds a file here and rewrites this list;
//! `cargo run -- migrate` applies the migrations the database has not had yet.

use cadmus::migrations::Migration;

/// Every migration, in the order they apply.
pub fn all() -> Vec<Migration> {
    Vec::new()
}
",
    )?;
    write_new(
        &root.join("src/routes.rs"),
        r#"//! Your own routes, served next to the generated admin and REST API.

use axum::Router;
use axum::routing::get;

pub fn router() -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}
"#,
    )?;
    write_new(
        &root.join("src/models/mod.rs"),
        &format!(
            r"//! Models. `cadmus generate model` adds a module and a registration line here.

use cadmus::App;

{MODULES_MARKER}

/// Registers every model: each gets its table, admin pages and REST endpoints.
pub fn register(app: App) -> App {{
    app
    {MODELS_MARKER}
}}
"
        ),
    )?;
    let env_example = &format!(
        r"# Secrets and values that differ between machines. Everything else is in
# src/settings.rs. Copy to .env and fill in; never commit .env.
DATABASE_URL={url}
# Signs login tokens. `cadmus secret` prints a new one.
SECRET_KEY=
# On your own machine: lets the login cookie work over plain http://.
# Leave it unset in production.
DEBUG=true
# Where people reach the site; needed to log in with Google and the like.
PUBLIC_URL=http://localhost:3000
# Sending e-mail (login links, password resets). Unset: messages go to the log.
# MAIL_URL=smtps://user:password@smtp.example.com
# Apps registered with the login providers enabled in settings.rs:
# GOOGLE_CLIENT_ID=
# GOOGLE_CLIENT_SECRET=
# Other client IDs (mobile apps) whose Google ID tokens the API accepts:
# GOOGLE_AUDIENCES=
# Your front end, if people log in to it through the API:
# APP_URL=http://localhost:5173
# LOG_LEVEL=debug
# Requests per caller, e.g. 120/min, 10/s, 5000/hour:
# RATE_LIMIT_ANONYMOUS=120/min
# RATE_LIMIT_LOGGED_IN=600/min
# Credentials for the storage backend named in settings.rs, if any:
# AWS_ACCESS_KEY_ID=
# AWS_SECRET_ACCESS_KEY=
",
        url = database.url(name),
    );
    write_new(&root.join(".env.example"), env_example)?;
    write_new(
        &root.join(".env"),
        &env_example.replace("SECRET_KEY=", &format!("SECRET_KEY={}", secret())),
    )?;
    write_new(&root.join("AGENTS.md"), &agents_guide(name))?;
    write_new(
        &root.join(".gitignore"),
        "/target\n.env\n*.sqlite3\n/media\n",
    )?;

    Ok(format!(
        "created {}\n\
         next:\n  \
         cd {name}\n  \
         cadmus generate model Post title:string body:text published:bool\n  \
         cargo run -- makemigrations\n  \
         cargo run -- migrate\n  \
         cargo run -- createsuperuser --username admin\n  \
         cargo run -- serve      # status page at /, admin at /admin, API docs at /api/docs",
        root.display()
    ))
}

/// The generated `AGENTS.md`: how to work on the project, for AI coding agents.
fn agents_guide(name: &str) -> String {
    format!(
        r#"# {name}

A [Cadmus](https://arnabgupta-dev.github.io/cadmus/) project: a Rust web app where each model
gets a database table, admin pages and REST API endpoints.

## Layout

- `src/models/`: one file per model (a SeaORM entity). `mod.rs` registers each model with its
  admin options (`ModelOptions`); keep the two `// cadmus:` marker comments in it.
- `src/migrations/`: written by `makemigrations`. Never edit a migration that has been applied;
  make a new one.
- `src/settings.rs`: every setting. Secrets come from `.env`, which is not in git.
- `src/routes.rs`: your own axum routes, next to the generated ones.

## Workflow

1. Add a model: `cadmus generate model Book title:string isbn:string? author:references`
   (generate a referenced model before the models that point to it). Field types: string,
   text, int, bigint, float, bool, datetime, date, json, decimal, uuid, html (formatted text,
   edited with a rich-text editor), file, references,
   many (`tags:many`, many-to-many), owner (`owner:owner`, rows belong to accounts); `?` makes
   a field optional.
   Checks on save go in the model's `ActiveModelBehavior` (`before_save`), returning
   `cadmus::invalid("field", "message")` to refuse a value.
2. Or edit a model file by hand. Mark a renamed field `#[sea_orm(renamed_from = "old_name")]`
   so its data is kept. To remove a model, `cadmus delete model Book --yes` (models that
   reference it first); the next migration drops its table and rows.
3. `cargo run -- makemigrations --no-input`, then `cargo run -- migrate`.
4. `cargo run -- check` validates the settings; `cargo build` must pass with no warnings.
5. `cargo run -- serve` starts the site: admin at /admin, API documentation at /api/docs.

## Documentation

- Guides: https://arnabgupta-dev.github.io/cadmus/docs/
- Everything in one file: https://arnabgupta-dev.github.io/cadmus/llms-full.txt
- API reference: https://docs.rs/cadmus
- With the MCP server (`cadmus mcp`), search the docs with the `docs_search` tool.
"#
    )
}

/// The generated `src/settings.rs`.
fn settings_source(name: &str) -> String {
    format!(
        r##"//! Project settings: how the site behaves, in one place.
//!
//! How the project behaves is decided here, in code, where the compiler checks every name.
//! Only secrets and values that differ between machines come from the environment (`.env`
//! on your machine, the host's variables in production), read with `env(...)`.
//!
//! Every field is documented: hover over it in your editor, or see the `Settings` type in
//! the cadmus documentation. Anything left out keeps its default.

use cadmus::AdminError;
use cadmus::settings::*;

pub fn settings() -> Result<Settings, AdminError> {{
    // `DEBUG=true` in .env on your machine; unset in production.
    let debug = env_or("DEBUG", false)?;

    Ok(Settings {{
        secret_key: env("SECRET_KEY")?,
        database: DatabaseSettings {{
            // The address picks the database: sqlite://db.sqlite3?mode=rwc,
            // postgres://user:password@host/name or mysql://user:password@host/name
            // (MariaDB too). It usually holds a password, so it lives in .env.
            url: env("DATABASE_URL")?,
            ..Default::default()
        }},
        server: ServerSettings {{
            bind: env_or("APP_BIND", "127.0.0.1:3000".to_owned())?,
            // Browser apps on other origins that may call the API:
            // cors_origins: vec!["https://app.example.com".into()],
            // Behind a reverse proxy, the header it puts the client's address in:
            // client_ip_header: Some("X-Forwarded-For".into()),
            // Where people reach the site; needed to log in with Google and the like.
            public_url: env_opt("PUBLIC_URL"),
            ..Default::default()
        }},
        auth: AuthSettings {{
            // How people log in, in the order the login page shows them; apps use the same
            // methods through the REST API (/api/auth/...). Each other site needs an app
            // registered with it, whose redirect URI is <PUBLIC_URL>/admin/auth/<name>/callback,
            // e.g. .../admin/auth/google/callback, plus your app's own addresses.
            methods: vec![
                AuthMethod::Password,
                // A login link by e-mail (see `mail` below):
                // AuthMethod::MagicLink,
                // AuthMethod::Google(OAuthClient::from_env("GOOGLE")?),
                // AuthMethod::Microsoft {{
                //     client: OAuthClient::from_env("MICROSOFT")?,
                //     tenant: "common".into(),
                // }},
                // AuthMethod::GitHub(OAuthClient::from_env("GITHUB")?),
                // AuthMethod::Facebook(OAuthClient::from_env("FACEBOOK")?),
                // AuthMethod::Oidc(OidcProvider::new(
                //     "okta", "Okta", "https://acme.okta.com", OAuthClient::from_env("OKTA")?,
                // )),
            ],
            // Who may get an account by logging in through another site, or by signing up in
            // an app. By default only existing accounts, matched by e-mail address, may log
            // in. Accounts made through the admin's login are staff; through the API, not.
            // signup: Signup::Domains(vec!["acme.com".into()]),
            // signup_groups: vec!["Editors".into()],
            // The login cookie needs HTTPS, except on your own machine.
            cookie_secure: !debug,
            // Require two-factor authentication for every account:
            // totp_required: true,
            // Accounts can make API keys for programs calling the REST API; to refuse them:
            // api_keys: false,
            // An app whose users log in through the REST API: where e-mailed links (login,
            // sign-up, password reset) point, and where providers may send people back.
            // app: AppSettings {{
            //     url: env_opt("APP_URL"),
            //     redirect_uris: vec!["http://localhost:5173/auth/callback".into()],
            //     ..Default::default()
            // }},
            ..Default::default()
        }},
        admin: AdminSettings {{
            site_title: "{name}".into(),
            // accent_color: "#0a7f5a".into(),
            // theme: Theme::Dark,
            ..Default::default()
        }},
        docs: DocsSettings {{
            // API documentation at /api/docs. Ask for a password before showing it:
            // username: Some("docs".into()),
            // password: Some(env("DOCS_PASSWORD")?),
            // or turn it off:
            // enabled: false,
            ..Default::default()
        }},
        home: HomeSettings {{
            // The public status page at /.
            title: Some("{name}".into()),
            ..Default::default()
        }},
        uploads: UploadSettings {{
            // Turn on file uploads: file:///absolute/path, s3://bucket/prefix,
            // az://container/prefix or gs://bucket/prefix.
            // storage_url: Some("file:///var/lib/{name}/media".into()),
            ..Default::default()
        }},
        mail: MailSettings {{
            // Login links and password resets. Until MAIL_URL is set they are written to
            // the log, so on your machine you can copy them from the terminal.
            url: env_opt("MAIL_URL"),
            from: Some("{name} <noreply@example.com>".into()),
        }},
        log_level: env_or("LOG_LEVEL", LogLevel::INFO)?,
        rate_limit: RateLimitSettings {{
            // Requests each caller may make; beyond that the answer is 429.
            anonymous: env_or("RATE_LIMIT_ANONYMOUS", Rate::per_minute(120))?,
            logged_in: env_or("RATE_LIMIT_LOGGED_IN", Rate::per_minute(600))?,
            // Limits for some paths instead:
            // rules: vec![RateRule::new("/api/orders", Rate::per_minute(10)).methods(["POST"])],
            // or turn limits off:
            // enabled: false,
            ..Default::default()
        }},
        ..Default::default()
    }})
}}
"##
    )
}

/// One parsed `name:type[?]` field.
#[derive(Debug, PartialEq)]
struct FieldSpec {
    name: String,
    rust_type: &'static str,
    attribute: Option<&'static str>,
    nullable: bool,
    /// For a foreign key: the module of the model it points to.
    references: Option<String>,
    /// Holds an uploaded file's id, shown as a file picker in the admin.
    file: bool,
    /// Holds the username of the account the row belongs to (`ModelOptions::owned_by`).
    owner: bool,
    /// Formatted text, edited with the rich-text editor (`ModelOptions::rich_text`).
    html: bool,
    /// A many-to-many field: the module of the model it links to. Not a column; it gets a
    /// link table of its own.
    many: Option<String>,
}

/// `blog_post` -> `BlogPost`.
fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Words that cannot be used as field names in generated Rust.
const KEYWORDS: [&str; 24] = [
    "as", "async", "await", "box", "const", "crate", "dyn", "enum", "fn", "for", "if", "impl",
    "loop", "match", "mod", "move", "pub", "ref", "self", "struct", "trait", "type", "use",
    "where",
];

fn parse_field(spec: &str) -> Result<FieldSpec, String> {
    let (name, kind) = spec
        .split_once(':')
        .ok_or_else(|| format!("`{spec}`: fields look like name:type"))?;
    let valid_name = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_name || KEYWORDS.contains(&name) {
        return Err(format!(
            "`{name}`: field names are snake_case and must not be Rust keywords"
        ));
    }
    if name == "id" {
        return Err("`id` is generated automatically as the primary key".into());
    }
    let (kind, nullable) = match kind.strip_suffix('?') {
        Some(base) => (base, true),
        None => (kind, false),
    };
    // `author:references` points at the model `author`; `editor:references=author` gives the
    // column another name than the model it points to.
    let reference = match kind.split_once('=') {
        Some(("references", target)) => Some(target),
        None if kind == "references" => Some(name),
        _ => None,
    };
    if let Some(target) = reference {
        let valid_target = target
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
            && target
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid_target {
            return Err(format!(
                "`{spec}`: name the model to point to in snake_case, e.g. author:references or editor:references=author"
            ));
        }
        return Ok(FieldSpec {
            name: format!("{name}_id"),
            rust_type: "i32",
            attribute: None,
            nullable,
            references: Some(target.to_owned()),
            file: false,
            owner: false,
            html: false,
            many: None,
        });
    }
    // `tags:many` links to the model `tag`; `readers:many=user` names it.
    let many = match kind.split_once('=') {
        Some(("many", target)) => Some(target.to_owned()),
        None if kind == "many" => Some(name.strip_suffix('s').unwrap_or(name).to_owned()),
        _ => None,
    };
    if let Some(target) = many {
        let valid_target = target
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
            && target
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid_target || nullable {
            return Err(format!(
                "`{spec}`: name the model to link to in snake_case, e.g. tags:many or readers:many=user"
            ));
        }
        return Ok(FieldSpec {
            name: name.to_owned(),
            rust_type: "",
            attribute: None,
            nullable: false,
            references: None,
            file: false,
            owner: false,
            html: false,
            many: Some(target),
        });
    }
    if kind == "owner" {
        return Ok(FieldSpec {
            name: name.to_owned(),
            rust_type: "String",
            attribute: None,
            nullable: false,
            references: None,
            file: false,
            owner: true,
            html: false,
            many: None,
        });
    }
    // An uploaded file: its id, always optional, so rows can exist before a file is picked.
    if kind == "file" {
        return Ok(FieldSpec {
            name: name.to_owned(),
            rust_type: "i32",
            attribute: None,
            nullable: true,
            references: None,
            file: true,
            owner: false,
            html: false,
            many: None,
        });
    }
    let (rust_type, attribute) = match kind {
        "string" => ("String", None),
        "text" | "html" => ("String", Some("#[sea_orm(column_type = \"Text\")]")),
        "int" => ("i32", None),
        "bigint" => ("i64", None),
        "float" => ("f64", None),
        "bool" => ("bool", None),
        "datetime" => ("DateTime", None),
        "date" => ("Date", None),
        "json" => ("Json", None),
        "decimal" => (
            "Decimal",
            Some("#[sea_orm(column_type = \"Decimal(Some((12, 2)))\")]"),
        ),
        "uuid" => ("Uuid", None),
        other => {
            return Err(format!(
                "`{other}`: unknown type (use string, text, int, bigint, float, bool, datetime, date, json, decimal, uuid, html, file, references, many, owner)"
            ));
        }
    };
    Ok(FieldSpec {
        name: name.to_owned(),
        rust_type,
        attribute,
        nullable,
        references: None,
        file: false,
        owner: false,
        html: kind == "html",
        many: None,
    })
}

/// `BlogPost` -> `blog_post`.
fn snake_case(pascal: &str) -> String {
    let mut out = String::new();
    for (i, c) in pascal.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// `BlogPost` -> `Blog posts`, `Category` -> `Categories`.
fn label(pascal: &str) -> String {
    let words = snake_case(pascal).replace('_', " ");
    let mut chars = words.chars();
    let capitalised: String = match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => words.clone(),
    };
    // Enough English to avoid "Categorys" and "Boxs"; anything irregular can be changed in
    // the generated `.label(...)` call.
    let vowel_before_y = capitalised
        .chars()
        .rev()
        .nth(1)
        .is_some_and(|c| "aeiou".contains(c));
    if let Some(stem) = capitalised.strip_suffix('y').filter(|_| !vowel_before_y) {
        format!("{stem}ies")
    } else if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|ending| capitalised.ends_with(ending))
    {
        format!("{capitalised}es")
    } else {
        format!("{capitalised}s")
    }
}

fn render_model(table: &str, fields: &[FieldSpec]) -> String {
    // `f64` has no total equality, so `Eq` can only be derived without float fields.
    let eq = if fields
        .iter()
        .any(|f| f.rust_type == "f64" || f.rust_type == "Json")
    {
        ""
    } else {
        ", Eq"
    };
    let mut body = String::new();
    for field in fields.iter().filter(|f| f.many.is_none()) {
        if let Some(attribute) = field.attribute {
            let _ = writeln!(body, "    {attribute}");
        }
        let ty = if field.nullable {
            format!("Option<{}>", field.rust_type)
        } else {
            field.rust_type.to_owned()
        };
        let _ = writeln!(body, "    pub {}: {ty},", field.name);
    }
    // One `belongs_to` per foreign key. The admin reads these to offer a dropdown, and
    // `makemigrations` to create the constraint.
    let mut relations = String::new();
    for field in fields {
        let Some(target) = &field.references else {
            continue;
        };
        // A model may point to itself, e.g. a category's parent category.
        let path = if target == table {
            String::new()
        } else {
            format!("super::{target}::")
        };
        let column = pascal_case(&field.name);
        let variant = pascal_case(field.name.trim_end_matches("_id"));
        let _ = writeln!(
            relations,
            "    #[sea_orm(\n        belongs_to = \"{path}Entity\",\n        from = \"Column::{column}\",\n        to = \"{path}Column::Id\"\n    )]\n    {variant},"
        );
    }
    let relations = if relations.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{\n{relations}}}")
    };
    format!(
        r#"use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq{eq}, DeriveEntityModel)]
#[sea_orm(table_name = "{table}")]
pub struct Model {{
    #[sea_orm(primary_key)]
    pub id: i32,
{body}}}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {relations}

// Checks and changes made on every save or delete (`before_save`, `before_delete`...) go
// here; see "Checks and hooks" in the Models guide.
impl ActiveModelBehavior for ActiveModel {{}}
"#
    )
}

/// The table behind a many-to-many field: one row per link, removed with either side.
fn render_link_table(table: &str, from: &str, to_column: &str, target: &str) -> String {
    let from_col = pascal_case(&format!("{from}_id"));
    let to_col = pascal_case(to_column);
    format!(
        r#"//! Links for a many-to-many field: one row per pair. Managed by the admin and the API.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "{table}")]
pub struct Model {{
    #[sea_orm(primary_key)]
    pub id: i32,
    pub {from}_id: i32,
    pub {to_column}: i32,
}}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {{
    #[sea_orm(
        belongs_to = "super::{from}::Entity",
        from = "Column::{from_col}",
        to = "super::{from}::Column::Id",
        on_delete = "Cascade"
    )]
    From,
    #[sea_orm(
        belongs_to = "super::{target}::Entity",
        from = "Column::{to_col}",
        to = "super::{target}::Column::Id",
        on_delete = "Cascade"
    )]
    To,
}}

impl ActiveModelBehavior for ActiveModel {{}}
"#
    )
}

fn generate_model(project: &Path, name: &str, fields: &[String]) -> Result<String, String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err("model names are PascalCase letters and digits, e.g. BlogPost".into());
    }
    let module = snake_case(name);
    if KEYWORDS.contains(&module.as_str()) {
        return Err(format!(
            "`{name}` would produce the Rust keyword `{module}`"
        ));
    }
    let mut parsed = Vec::new();
    for spec in fields {
        let field = parse_field(spec)?;
        if parsed.iter().any(|f: &FieldSpec| f.name == field.name) {
            return Err(format!("field `{}` is listed twice", field.name));
        }
        parsed.push(field);
    }

    let index = project.join("src/models/mod.rs");
    let existing = std::fs::read_to_string(&index).map_err(|_| {
        "src/models/mod.rs not found: run this inside a project created by `cadmus new`".to_owned()
    })?;
    if !existing.contains(MODULES_MARKER) || !existing.contains(MODELS_MARKER) {
        return Err("src/models/mod.rs has lost its `// cadmus:` marker comments".into());
    }
    // Check before writing anything, so a failure leaves the project untouched.
    let file = project.join(format!("src/models/{module}.rs"));
    if file.exists() {
        return Err(format!("{} already exists", file.display()));
    }
    for target in parsed.iter().filter_map(|f| f.references.as_ref()) {
        if *target != module && !project.join(format!("src/models/{target}.rs")).is_file() {
            return Err(format!(
                "there is no model `{target}` to reference: generate it first (src/models/{target}.rs)"
            ));
        }
    }
    // Each many-to-many field gets a link table, `<model>_<field>`.
    let mut link_tables = Vec::new();
    for field in &parsed {
        let Some(target) = &field.many else {
            continue;
        };
        if *target != module && !project.join(format!("src/models/{target}.rs")).is_file() {
            return Err(format!(
                "there is no model `{target}` to link to: generate it first (src/models/{target}.rs)"
            ));
        }
        let table = format!("{module}_{}", field.name);
        let path = project.join(format!("src/models/{table}.rs"));
        if path.exists() || existing.contains(&format!("pub mod {table};")) {
            return Err(format!("{} already exists", path.display()));
        }
        // A model linking to itself needs two different column names.
        let to_column = if *target == module {
            format!("linked_{target}_id")
        } else {
            format!("{target}_id")
        };
        link_tables.push((field.name.clone(), table, path, to_column, target.clone()));
    }

    let files: Vec<String> = parsed
        .iter()
        .filter(|f| f.file)
        .map(|f| format!("{:?}", f.name))
        .collect();
    let mut file_option = if files.is_empty() {
        String::new()
    } else {
        format!(".file_fields([{}])", files.join(", "))
    };
    let rich: Vec<String> = parsed
        .iter()
        .filter(|f| f.html)
        .map(|f| format!("{:?}", f.name))
        .collect();
    if !rich.is_empty() {
        let _ = write!(file_option, ".rich_text([{}])", rich.join(", "));
    }
    if let Some(owner) = parsed.iter().find(|f| f.owner) {
        let _ = write!(file_option, ".owned_by({:?})", owner.name);
    }
    for (field, table, _, to_column, _) in &link_tables {
        let _ = write!(
            file_option,
            ".many_to_many::<{table}::Entity>({field:?}, {table}::Column::{}, {table}::Column::{})",
            pascal_case(&format!("{module}_id")),
            pascal_case(to_column)
        );
    }
    let link_modules: String = link_tables
        .iter()
        .map(|(_, table, ..)| format!("pub mod {table};\n"))
        .collect();
    let updated = existing
        .replace(
            MODULES_MARKER,
            &format!("pub mod {module};\n{link_modules}{MODULES_MARKER}"),
        )
        .replace(
            MODELS_MARKER,
            &format!(
                "    .model::<{module}::Entity>(cadmus::ModelOptions::new().label({:?}){file_option})\n    {MODELS_MARKER}",
                label(name)
            ),
        );
    write_new(&file, &render_model(&module, &parsed))?;
    for (_, table, path, to_column, target) in &link_tables {
        write_new(path, &render_link_table(table, &module, to_column, target))?;
    }
    std::fs::write(&index, updated).map_err(|e| format!("{}: {e}", index.display()))?;

    let mut report = format!("created {}\n", file.display());
    for (field, _, path, ..) in &link_tables {
        let _ = writeln!(report, "created {} for the field `{field}`", path.display());
    }
    let _ = writeln!(report, "registered {name} in src/models/mod.rs");
    if !files.is_empty() {
        report.push_str("file fields need uploads: set uploads.storage_url in src/settings.rs\n");
    }
    report.push_str("next: cargo run -- makemigrations && cargo run -- migrate");
    Ok(report)
}

/// What `delete model` will change, worked out before anything is touched.
struct ModelDeletion {
    name: String,
    /// The module and table name, e.g. `blog_post`.
    module: String,
    file: PathBuf,
    index: PathBuf,
    /// `src/models/mod.rs` without the model's `pub mod` line and registration.
    updated_index: String,
    /// The registration listed file fields, whose uploads stay in storage.
    had_files: bool,
    /// Link tables of the model's many-to-many fields, deleted with it.
    link_files: Vec<PathBuf>,
}

/// Checks that a model can be removed cleanly: its file exists, both its entries in
/// `src/models/mod.rs` are found, and no other code uses it. Changes nothing.
fn plan_model_deletion(project: &Path, name: &str) -> Result<ModelDeletion, String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err("model names are PascalCase letters and digits, e.g. BlogPost".into());
    }
    let module = snake_case(name);
    let index = project.join("src/models/mod.rs");
    let existing = std::fs::read_to_string(&index).map_err(|_| {
        "src/models/mod.rs not found: run this inside a project created by `cadmus new`".to_owned()
    })?;
    let file = project.join(format!("src/models/{module}.rs"));
    if !file.is_file() {
        return Err(format!(
            "there is no model `{name}`: src/models/{module}.rs does not exist"
        ));
    }
    let (mut updated_index, registration) = unregister(&existing, &module).ok_or_else(|| {
        format!(
            "src/models/mod.rs: could not find both `pub mod {module};` and \
             `.model::<{module}::Entity>(...)`; remove them and src/models/{module}.rs by hand"
        )
    })?;
    // The link tables of its many-to-many fields go with it.
    let mut link_files = Vec::new();
    for (at, pattern) in registration.match_indices(".many_to_many::<") {
        let rest = &registration[at + pattern.len()..];
        let Some((link, _)) = rest.split_once("::Entity>") else {
            continue;
        };
        let link = link.trim();
        if let Some((start, end)) = declaration(&updated_index, link) {
            updated_index = cut(&updated_index, start, end);
        }
        let path = project.join(format!("src/models/{link}.rs"));
        if path.is_file() {
            link_files.push(path);
        }
    }

    // Anything still naming the module would stop the project from compiling.
    let mut uses = Vec::new();
    if let Some(line) = first_use(&updated_index, &module) {
        uses.push(format!("src/models/mod.rs, line {line}"));
    }
    let src = project.join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();
    for path in files {
        // Migrations describe tables on their own and never name a model.
        if path == file
            || path == index
            || link_files.contains(&path)
            || path.starts_with(src.join("migrations"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(line) = first_use(&text, &module) {
            let shown = path.strip_prefix(project).unwrap_or(&path);
            uses.push(format!("{}, line {line}", shown.display()));
        }
    }
    if !uses.is_empty() {
        return Err(format!(
            "{name} is still used in:\n  {}\nremove those uses first (for a foreign key to {name}, \
             delete that field or its model), then run this again",
            uses.join("\n  ")
        ));
    }
    Ok(ModelDeletion {
        name: name.to_owned(),
        had_files: registration.contains(".file_fields("),
        link_files,
        module,
        file,
        index,
        updated_index,
    })
}

/// Removes a model as planned by [`plan_model_deletion`].
fn delete_model(plan: &ModelDeletion) -> Result<String, String> {
    // The registration goes first: a model file that is no longer a module is just ignored,
    // while a module without its file would not compile.
    std::fs::write(&plan.index, &plan.updated_index)
        .map_err(|e| format!("{}: {e}", plan.index.display()))?;
    std::fs::remove_file(&plan.file).map_err(|e| format!("{}: {e}", plan.file.display()))?;
    let mut report = format!("deleted {}\n", plan.file.display());
    for link in &plan.link_files {
        std::fs::remove_file(link).map_err(|e| format!("{}: {e}", link.display()))?;
        let _ = writeln!(report, "deleted {}", link.display());
    }
    let _ = writeln!(report, "removed {} from src/models/mod.rs", plan.name);
    if plan.had_files {
        report.push_str("files uploaded to it stay in storage\n");
    }
    let _ = write!(
        report,
        "next: cargo run -- makemigrations && cargo run -- migrate\n\
         the migration drops the table `{}` and every row in it",
        plan.module
    );
    if !plan.link_files.is_empty() {
        report.push_str(", with its many-to-many links");
    }
    Ok(report)
}

/// `src/models/mod.rs` without `pub mod <module>;` and `.model::<<module>::Entity>(...)`,
/// with the removed registration; `None` if either is missing.
fn unregister(source: &str, module: &str) -> Option<(String, String)> {
    let declared = declaration(source, module)?;

    let needle = format!(".model::<{module}::Entity>");
    let start = source.find(&needle)?;
    let open = start
        + needle.len()
        + (source[start + needle.len()..].len()
            - source[start + needle.len()..].trim_start().len());
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    // The matching `)`, skipping any inside string literals such as labels.
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = None;
    for (i, byte) in source.bytes().enumerate().skip(open) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let registered = (start, end?);
    let removed = source[registered.0..registered.1].to_owned();

    // Cut the later span first so the earlier one's offsets stay valid.
    let (first, second) = if declared.0 < registered.0 {
        (declared, registered)
    } else {
        (registered, declared)
    };
    let text = cut(source, second.0, second.1);
    Some((cut(&text, first.0, first.1), removed))
}

/// Where `pub mod <module>;` is in `source`.
fn declaration(source: &str, module: &str) -> Option<(usize, usize)> {
    let declaration = format!("pub mod {module};");
    let mut offset = 0;
    let mut declared = None;
    for line in source.split_inclusive('\n') {
        if line.trim() == declaration {
            let start = offset + (line.len() - line.trim_start().len());
            declared = Some((start, start + declaration.len()));
        }
        offset += line.len();
    }
    declared
}

/// Removes `start..end`, and its whole line when nothing else is on it.
fn cut(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
    if text[line_start..start].trim().is_empty() && text[end..line_end].trim().is_empty() {
        format!("{}{}", &text[..line_start], &text[line_end..])
    } else {
        format!("{}{}", &text[..start], &text[end..])
    }
}

/// The first line (from 1) that names `<module>::` outside a comment, as in
/// `super::author::Entity`.
fn first_use(text: &str, module: &str) -> Option<usize> {
    let needle = format!("{module}::");
    text.lines().enumerate().find_map(|(number, line)| {
        if line.trim_start().starts_with("//") {
            return None;
        }
        let found = line.match_indices(&needle).any(|(at, _)| {
            !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        found.then_some(number + 1)
    })
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Asks a yes-or-no question on the terminal; anything but yes is no.
fn confirm(question: &str) -> bool {
    use std::io::Write as _;
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cadmus-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_field_specs() {
        assert_eq!(
            parse_field("title:string").unwrap(),
            FieldSpec {
                name: "title".into(),
                rust_type: "String",
                attribute: None,
                nullable: false,
                references: None,
                file: false,
                owner: false,
                html: false,
                many: None,
            }
        );
        // A file is an optional id, registered as a file field.
        let photo = parse_field("photo:file").unwrap();
        assert!(photo.file && photo.nullable);
        assert_eq!(photo.rust_type, "i32");
        let author = parse_field("author:references").unwrap();
        assert_eq!(
            (
                author.name.as_str(),
                author.rust_type,
                author.references.as_deref()
            ),
            ("author_id", "i32", Some("author"))
        );
        let editor = parse_field("editor:references=blog_user?").unwrap();
        assert_eq!(editor.name, "editor_id");
        assert!(editor.nullable && editor.references.as_deref() == Some("blog_user"));
        let rating = parse_field("rating:float?").unwrap();
        assert!(rating.nullable && rating.rust_type == "f64");
        assert!(parse_field("body:text").unwrap().attribute.is_some());
    }

    #[test]
    fn rejects_bad_field_specs() {
        for bad in [
            "title",
            "title:varchar",
            "Title:string",
            "1st:int",
            "type:string",
            "id:int",
            "a b:int",
            ":int",
            "x:",
            "author:references=",
            "author:references=Author",
            "author:int=author",
        ] {
            assert!(parse_field(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn names_are_converted() {
        assert_eq!(snake_case("BlogPost"), "blog_post");
        assert_eq!(label("BlogPost"), "Blog posts");
        assert_eq!(label("Category"), "Categories");
        assert_eq!(label("Day"), "Days");
        assert_eq!(label("Box"), "Boxes");
        assert_eq!(label("Address"), "Addresses");
        assert_eq!(label("Branch"), "Branches");
        assert_eq!(pascal_case("blog_post_id"), "BlogPostId");
    }

    #[test]
    fn renders_entity_source() {
        let fields = [
            parse_field("title:string").unwrap(),
            parse_field("score:float?").unwrap(),
        ];
        let source = render_model("post", &fields);
        assert!(source.contains("#[sea_orm(table_name = \"post\")]"));
        assert!(source.contains("pub title: String,"));
        assert!(source.contains("pub score: Option<f64>,"));
        assert!(!source.contains(", Eq"), "float fields cannot derive Eq");
        assert!(render_model("tag", &[parse_field("name:string").unwrap()]).contains(", Eq"));
        assert!(source.contains("pub enum Relation {}"));

        let fields = [
            parse_field("author:references").unwrap(),
            parse_field("parent:references=post?").unwrap(),
        ];
        let source = render_model("post", &fields);
        assert!(
            source.contains("pub author_id: i32,")
                && source.contains("pub parent_id: Option<i32>,")
        );
        assert!(source.contains("belongs_to = \"super::author::Entity\""));
        assert!(source.contains("from = \"Column::AuthorId\"") && source.contains("    Author,"));
        // A model pointing to itself names its own entity.
        assert!(
            source.contains("belongs_to = \"Entity\"") && source.contains("to = \"Column::Id\"")
        );
        assert!(source.contains("    Parent,"));
    }

    #[test]
    fn new_project_starts_with_the_chosen_database() {
        let dir = temp("database");
        let read = |project: &str, file: &str| {
            std::fs::read_to_string(dir.join(project).join(file)).unwrap()
        };

        new_project(&dir, "lite", None, Database::Sqlite).unwrap();
        assert!(
            read("lite", "Cargo.toml").contains(&format!("cadmus = \"{}\"\n", framework_version()))
        );
        assert!(read("lite", ".env").contains("DATABASE_URL=sqlite://db.sqlite3?mode=rwc\n"));

        new_project(&dir, "my-shop", None, Database::Postgres).unwrap();
        // Every driver is built in, so only the address differs.
        assert_eq!(
            read("my-shop", "Cargo.toml"),
            read("lite", "Cargo.toml").replace("\"lite\"", "\"my-shop\"")
        );
        for file in [".env", ".env.example"] {
            assert!(
                read("my-shop", file)
                    .contains("DATABASE_URL=postgres://postgres:postgres@localhost:5432/my_shop\n"),
                "{file}"
            );
        }

        new_project(&dir, "legacy", None, Database::Mysql).unwrap();
        assert!(!read("legacy", "Cargo.toml").contains("features = [\"mysql\"]"));
        assert!(
            read("legacy", ".env")
                .contains("DATABASE_URL=mysql://root:root@localhost:3306/legacy\n")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_project_then_generate_model() {
        let dir = temp("project");
        new_project(&dir, "blog", None, Database::Sqlite).unwrap();
        let root = dir.join("blog");
        for file in [
            "Cargo.toml",
            "src/main.rs",
            "src/routes.rs",
            "src/models/mod.rs",
            "src/settings.rs",
            "src/migrations/mod.rs",
            "AGENTS.md",
            ".env",
            ".env.example",
            ".gitignore",
        ] {
            assert!(root.join(file).is_file(), "{file} missing");
        }
        // The real secret is only in the ignored file.
        let env = std::fs::read_to_string(root.join(".env")).unwrap();
        let secret_line = env.lines().find(|l| l.starts_with("SECRET_KEY=")).unwrap();
        assert_eq!(secret_line.len(), "SECRET_KEY=".len() + 64);
        assert!(
            std::fs::read_to_string(root.join(".env.example"))
                .unwrap()
                .contains("SECRET_KEY=\n")
        );
        let settings = std::fs::read_to_string(root.join("src/settings.rs")).unwrap();
        assert!(settings.contains(r#"site_title: "blog".into(),"#));
        assert!(settings.contains(r#"secret_key: env("SECRET_KEY")?,"#));
        assert!(
            std::fs::read_to_string(root.join("src/main.rs"))
                .unwrap()
                .contains(".migrations(migrations::all())")
        );
        assert!(
            std::fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains(".env")
        );

        generate_model(
            &root,
            "BlogPost",
            &["title:string".into(), "body:text".into()],
        )
        .unwrap();
        generate_model(&root, "Product", &["photo:file".into()]).unwrap();
        let index = std::fs::read_to_string(root.join("src/models/mod.rs")).unwrap();
        assert!(
            index.contains(".label(\"Products\").file_fields([\"photo\"]))"),
            "{index}"
        );
        assert!(index.contains("pub mod blog_post;"));
        assert!(index.contains(".model::<blog_post::Entity>("));
        assert!(root.join("src/models/blog_post.rs").is_file());

        // Refusals must leave the project exactly as it was.
        assert!(generate_model(&root, "BlogPost", &[]).is_err());
        assert!(generate_model(&root, "Tag", &["name:nope".into()]).is_err());
        assert!(generate_model(&root, "Tag", &["name:string".into(), "name:int".into()]).is_err());
        assert!(generate_model(&root, "lowercase", &[]).is_err());
        // A reference needs the other model to exist already.
        assert!(generate_model(&root, "Tag", &["owner:references".into()]).is_err());
        assert!(!root.join("src/models/tag.rs").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("src/models/mod.rs")).unwrap(),
            index
        );

        assert!(
            new_project(&dir, "blog", None, Database::Sqlite).is_err(),
            "must not overwrite a project"
        );
        assert!(new_project(&dir, "Bad Name", None, Database::Sqlite).is_err());
        assert!(new_project(&dir, "../escape", None, Database::Sqlite).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_model_undoes_generate_model() {
        let dir = temp("delete");
        new_project(&dir, "blog", None, Database::Sqlite).unwrap();
        let root = dir.join("blog");
        let index_path = root.join("src/models/mod.rs");
        let fresh = std::fs::read_to_string(&index_path).unwrap();
        generate_model(&root, "Author", &["name:string".into()]).unwrap();
        generate_model(
            &root,
            "BlogPost",
            &[
                "title:string".into(),
                "author:references".into(),
                "cover:file".into(),
            ],
        )
        .unwrap();
        // Hand-edited admin options: several lines, with a `)` inside a string.
        let edited = std::fs::read_to_string(&index_path).unwrap().replace(
            ".label(\"Blog posts\")",
            ".label(\"Posts (all)\")\n            .search_fields([\"title\"])",
        );
        std::fs::write(&index_path, &edited).unwrap();

        // Still referenced by a foreign key: refused, nothing changed.
        let error = plan_model_deletion(&root, "Author").err().unwrap();
        assert!(error.contains("src/models/blog_post.rs, line"), "{error}");
        assert!(root.join("src/models/author.rs").is_file());
        assert_eq!(std::fs::read_to_string(&index_path).unwrap(), edited);
        // Your own code counts too.
        let routes = root.join("src/routes.rs");
        let original_routes = std::fs::read_to_string(&routes).unwrap();
        std::fs::write(&routes, format!("{original_routes}\n// use crate::models::blog_post;\nfn f(_: crate::models::blog_post::Model) {{}}\n")).unwrap();
        let error = plan_model_deletion(&root, "BlogPost").err().unwrap();
        assert!(error.contains("src/routes.rs, line"), "{error}");
        std::fs::write(&routes, &original_routes).unwrap();

        assert!(
            plan_model_deletion(&root, "Tag")
                .err()
                .unwrap()
                .contains("no model `Tag`")
        );
        assert!(plan_model_deletion(&root, "blog_post").is_err());

        let report = delete_model(&plan_model_deletion(&root, "BlogPost").unwrap()).unwrap();
        assert!(report.contains("drops the table `blog_post`"), "{report}");
        assert!(report.contains("stay in storage"), "{report}");
        assert!(!root.join("src/models/blog_post.rs").exists());
        let index = std::fs::read_to_string(&index_path).unwrap();
        assert!(
            !index.contains("blog_post") && !index.contains("Posts (all)"),
            "{index}"
        );

        // In the right order, everything comes back out: mod.rs is as `cadmus new` wrote it.
        delete_model(&plan_model_deletion(&root, "Author").unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&index_path).unwrap(), fresh);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn many_to_many_owner_decimal_uuid_and_html_fields() {
        let dir = temp("many");
        new_project(&dir, "blog", None, Database::Sqlite).unwrap();
        let root = dir.join("blog");
        let index_path = root.join("src/models/mod.rs");
        let fresh = std::fs::read_to_string(&index_path).unwrap();
        generate_model(&root, "Tag", &["name:string".into()]).unwrap();
        // The target must exist first.
        assert!(generate_model(&root, "Post", &["labels:many=label".into()]).is_err());
        let report = generate_model(
            &root,
            "Post",
            &[
                "title:string".into(),
                "tags:many".into(),
                "owner:owner".into(),
                "price:decimal".into(),
                "code:uuid?".into(),
                "body:html".into(),
            ],
        )
        .unwrap();
        assert!(
            report.contains("post_tags.rs for the field `tags`"),
            "{report}"
        );
        let model = std::fs::read_to_string(root.join("src/models/post.rs")).unwrap();
        assert!(model.contains("pub owner: String,"), "{model}");
        assert!(model.contains("pub price: Decimal,"));
        assert!(model.contains("pub code: Option<Uuid>,"));
        assert!(
            model.contains("#[sea_orm(column_type = \"Text\")]\n    pub body: String,"),
            "{model}"
        );
        assert!(
            !model.contains("tags"),
            "a many-to-many field is not a column"
        );
        let link = std::fs::read_to_string(root.join("src/models/post_tags.rs")).unwrap();
        assert!(link.contains("pub post_id: i32,") && link.contains("pub tag_id: i32,"));
        assert!(link.contains("belongs_to = \"super::tag::Entity\""));
        let index = std::fs::read_to_string(&index_path).unwrap();
        assert!(index.contains("pub mod post_tags;"));
        assert!(index.contains(".owned_by(\"owner\")"), "{index}");
        assert!(index.contains(".rich_text([\"body\"])"), "{index}");
        assert!(index.contains(
            ".many_to_many::<post_tags::Entity>(\"tags\", post_tags::Column::PostId, post_tags::Column::TagId)"
        ));

        // The tag is still linked to, so it stays; the post takes its link table with it.
        let error = plan_model_deletion(&root, "Tag").err().unwrap();
        assert!(error.contains("src/models/post_tags.rs"), "{error}");
        let report = delete_model(&plan_model_deletion(&root, "Post").unwrap()).unwrap();
        assert!(report.contains("post_tags.rs"), "{report}");
        assert!(!root.join("src/models/post_tags.rs").exists());
        delete_model(&plan_model_deletion(&root, "Tag").unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&index_path).unwrap(), fresh);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn first_use_ignores_comments_and_longer_names() {
        assert_eq!(first_use("use super::post::Entity;", "post"), Some(1));
        assert_eq!(first_use("// post::Entity\nx", "post"), None);
        assert_eq!(first_use("blog_post::Entity", "post"), None);
        assert_eq!(first_use("a\n(post::Model)", "post"), Some(2));
    }
}
