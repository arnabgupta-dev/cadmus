//! `hera`: creates projects and models for heraapi.
//!
//! ```text
//! hera new blog
//! cd blog
//! hera generate model Post title:string body:text published:bool views:int rating:float?
//! cargo run -- migrate
//! cargo run -- createsuperuser --username admin
//! cargo run -- serve
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// Where generated projects get the framework from until it is on crates.io.
const REPOSITORY: &str = "https://github.com/arnabgupta-dev/heraapi";
const MODULES_MARKER: &str = "// hera:modules (generated modules are added above this line)";
const MODELS_MARKER: &str = "// hera:models (generated models are registered above this line)";

#[derive(Parser)]
#[command(
    name = "hera",
    about = "Project and model generator for heraapi",
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
        /// Use a local checkout of the framework instead of the GitHub repository.
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
    /// Print a random value suitable for ADMIN_JWT_SECRET.
    Secret,
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
        /// float, bool, datetime, date, json, and `references` for a foreign key to another
        /// model (`author:references`, or `editor:references=author` to name it differently).
        fields: Vec<String>,
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
        Command::Secret => {
            println!("{}", secret());
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
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
) -> Result<(), String> {
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
    let source = match framework_path {
        Some(path) => {
            let absolute = path
                .canonicalize()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            format!("path = {:?}", absolute.display().to_string())
        }
        None => format!("git = {REPOSITORY:?}"),
    };
    // Every driver is built in by default, so the database is chosen in settings.
    let framework = format!("{{ {source} }}");

    write_new(
        &root.join("Cargo.toml"),
        &format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
axum = "0.8"
heraapi = {framework}
sea-orm = {{ version = "2", default-features = false, features = ["macros", "with-json", "with-chrono"] }}
tokio = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
"#
        ),
    )?;
    write_new(
        &root.join("src/main.rs"),
        r"mod models;
mod routes;
mod settings;

use heraapi::{AdminError, App};

#[tokio::main]
async fn main() -> Result<(), AdminError> {
    let app = App::from_settings(settings::settings()?).await?;
    models::register(app).routes(routes::router()).run().await
}
",
    )?;
    write_new(&root.join("src/settings.rs"), &settings_source(name))?;
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
            r"//! Models. `hera generate model` adds a module and a registration line here.

use heraapi::App;

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
# Signs login tokens. `hera secret` prints a new one.
SECRET_KEY=
# On your own machine: lets the login cookie work over plain http://.
# Leave it unset in production.
DEBUG=true
# Where people reach the site; needed to log in with Google and the like.
PUBLIC_URL=http://localhost:3000
# Apps registered with the login providers enabled in settings.rs:
# GOOGLE_CLIENT_ID=
# GOOGLE_CLIENT_SECRET=
# LOG_LEVEL=debug
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
    write_new(
        &root.join(".gitignore"),
        "/target\n.env\n*.sqlite3\n/media\n",
    )?;

    println!("created {}", root.display());
    println!("next:");
    println!("  cd {name}");
    println!("  hera generate model Post title:string body:text published:bool");
    println!("  cargo run -- migrate");
    println!("  cargo run -- createsuperuser --username admin");
    println!(
        "  cargo run -- serve      # status page at /, admin at /admin, API docs at /api/docs"
    );
    Ok(())
}

/// The generated `src/settings.rs`.
fn settings_source(name: &str) -> String {
    format!(
        r##"//! Project settings, like Django's settings.py.
//!
//! How the project behaves is decided here, in code, where the compiler checks every name.
//! Only secrets and values that differ between machines come from the environment (`.env`
//! on your machine, the host's variables in production), read with `env(...)`.
//!
//! Every field is documented: hover over it in your editor, or see the `Settings` type in
//! the heraapi documentation. Anything left out keeps its default.

use heraapi::AdminError;
use heraapi::settings::*;

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
            // How people log in, in the order the login page shows them. Each other site
            // needs an app registered with it, whose redirect URI is
            // <PUBLIC_URL>/admin/auth/<name>/callback, e.g. .../admin/auth/google/callback.
            methods: vec![
                AuthMethod::Password,
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
            // Who may get an account by logging in through another site. By default only
            // existing accounts, matched by e-mail address, may log in.
            // signup: Signup::Domains(vec!["acme.com".into()]),
            // signup_groups: vec!["Editors".into()],
            // The login cookie needs HTTPS, except on your own machine.
            cookie_secure: !debug,
            // Require two-factor authentication for every account:
            // totp_required: true,
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
        log_level: env_or("LOG_LEVEL", LogLevel::INFO)?,
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
        });
    }
    let (rust_type, attribute) = match kind {
        "string" => ("String", None),
        "text" => ("String", Some("#[sea_orm(column_type = \"Text\")]")),
        "int" => ("i32", None),
        "bigint" => ("i64", None),
        "float" => ("f64", None),
        "bool" => ("bool", None),
        "datetime" => ("DateTime", None),
        "date" => ("Date", None),
        "json" => ("Json", None),
        other => {
            return Err(format!(
                "`{other}`: unknown type (use string, text, int, bigint, float, bool, datetime, date, json, references)"
            ));
        }
    };
    Ok(FieldSpec {
        name: name.to_owned(),
        rust_type,
        attribute,
        nullable,
        references: None,
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
    for field in fields {
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
    // `migrate` to create the constraint.
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

impl ActiveModelBehavior for ActiveModel {{}}
"#
    )
}

fn generate_model(project: &Path, name: &str, fields: &[String]) -> Result<(), String> {
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
        "src/models/mod.rs not found: run this inside a project created by `hera new`".to_owned()
    })?;
    if !existing.contains(MODULES_MARKER) || !existing.contains(MODELS_MARKER) {
        return Err("src/models/mod.rs has lost its `// hera:` marker comments".into());
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

    let updated = existing
        .replace(
            MODULES_MARKER,
            &format!("pub mod {module};\n{MODULES_MARKER}"),
        )
        .replace(
            MODELS_MARKER,
            &format!(
                "    .model::<{module}::Entity>(heraapi::ModelOptions::new().label({:?}))\n    {MODELS_MARKER}",
                label(name)
            ),
        );
    write_new(&file, &render_model(&module, &parsed))?;
    std::fs::write(&index, updated).map_err(|e| format!("{}: {e}", index.display()))?;

    println!("created {}", file.display());
    println!("registered {name} in src/models/mod.rs");
    println!("next: cargo run -- migrate");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hera-cli-{name}-{}", std::process::id()));
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
            }
        );
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
            read("lite", "Cargo.toml").contains(&format!("heraapi = {{ git = {REPOSITORY:?} }}"))
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
                .contains("App::from_settings(settings::settings()?)")
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
        let index = std::fs::read_to_string(root.join("src/models/mod.rs")).unwrap();
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
}
