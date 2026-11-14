//! `cadmus mcp`: a Model Context Protocol server over standard input and output, so AI coding
//! agents can read the documentation and create projects, models and migrations.
//!
//! Messages are JSON-RPC 2.0, one per line. Nothing but protocol messages may be written to
//! standard output, so everything else goes to standard error.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::docs;

/// Protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest a project command may run; the first build of a project takes a few minutes.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How much of a command's output is passed back, from the end.
const OUTPUT_LIMIT: usize = 12_000;

/// What the agent is told when it connects.
const INSTRUCTIONS: &str = "\
Cadmus is a Rust web framework. Each model (a SeaORM entity) gets a database table, pages in an \
admin site and REST API endpoints, with accounts, permissions and migrations built in.

To build an app:
1. new_project: creates the project folder.
2. generate_model: once per kind of record; generate a model before the models that reference it.
3. run makemigrations, then run migrate: creates the tables.
4. create_superuser: an account to log in with.
5. Tell the user to run `cargo run -- serve` in the project: the admin is at /admin, the API \
documentation at /api/docs.

After changing a model file by hand, run makemigrations and migrate again. To remove a model, \
use delete_model (models that reference it first), then makemigrations and migrate; that drops \
its table and rows, so ask the user first. Never edit a \
migration that has been applied; make a new one. Before writing custom code, search the \
documentation (docs_search, docs_read): admin options are ModelOptions in src/models/mod.rs, \
settings are in src/settings.rs, your own routes go in src/routes.rs.";

/// Serves until standard input closes.
pub(crate) fn serve() -> Result<(), String> {
    eprintln!("cadmus {}: MCP server ready", env!("CARGO_PKG_VERSION"));
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("reading standard input: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line) {
            writeln!(stdout, "{reply}").map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The reply to one line of input, if it needs one.
pub(crate) fn handle_line(line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => return Some(error(Value::Null, -32700, &format!("not JSON: {e}"))),
    };
    if let Value::Array(batch) = message {
        let replies: Vec<Value> = batch.iter().filter_map(handle_message).collect();
        return (!replies.is_empty()).then_some(Value::Array(replies));
    }
    handle_message(&message)
}

fn handle_message(message: &Value) -> Option<Value> {
    // Replies to requests we never send, and notifications, get no answer.
    let method = message.get("method")?.as_str()?;
    let id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(&params),
        "resources/list" => Ok(resources()),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "resources/read" => read_resource(&params),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        _ => Err((-32601, format!("unknown method `{method}`"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error(id, code, &message),
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false },
        },
        "serverInfo": {
            "name": "cadmus",
            "title": "Cadmus",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn tools() -> Value {
    let project = json!({
        "type": "string",
        "description": "Absolute path of the project folder (the one with Cargo.toml).",
    });
    json!([
        {
            "name": "docs_list",
            "title": "List the documentation",
            "description": "Lists every page of the Cadmus documentation with a one-line summary. Read a page with docs_read.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "docs_read",
            "title": "Read a documentation page",
            "description": "Returns one page of the Cadmus documentation as Markdown.",
            "inputSchema": {
                "type": "object",
                "properties": { "page": { "type": "string", "description": "The page's name from docs_list, e.g. `models`." } },
                "required": ["page"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "docs_search",
            "title": "Search the documentation",
            "description": "Finds the documentation sections that best match the words given, e.g. `list filter` or `api key`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words to look for." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Most sections to return (default 5)." },
                },
                "required": ["query"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "new_project",
            "title": "Create a project",
            "description": "Creates a new Cadmus project in a new folder `<directory>/<name>`, with settings, a .env file holding a fresh secret key, and an empty model list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "directory": { "type": "string", "description": "Absolute path of an existing folder to create the project in." },
                    "name": { "type": "string", "description": "Project name: lowercase letters, digits, `-` and `_`, starting with a letter." },
                    "database": { "type": "string", "enum": ["sqlite", "postgres", "mysql"], "description": "Database to start with (default sqlite, which needs nothing installed)." },
                    "framework_path": { "type": "string", "description": "Absolute path of a local Cadmus checkout to use instead of the crates.io version. Usually left out." },
                },
                "required": ["directory", "name"],
            },
        },
        {
            "name": "generate_model",
            "title": "Generate a model",
            "description": "Adds a model to a project: a file in src/models/ and a registration line in src/models/mod.rs. Field types: string, text, int, bigint, float, bool, datetime, date, json, file, and references (a foreign key: `author:references`, or `editor:references=author`). Add `?` to make a field optional, e.g. `rating:float?`. An `id` primary key is added automatically. Generate referenced models first. Afterwards, run makemigrations and migrate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project,
                    "name": { "type": "string", "description": "Model name in PascalCase, e.g. `BlogPost`." },
                    "fields": { "type": "array", "items": { "type": "string" }, "description": "Fields as `name:type`, e.g. [\"title:string\", \"body:text\", \"published:bool\"]." },
                },
                "required": ["project", "name", "fields"],
            },
        },
        {
            "name": "delete_model",
            "title": "Delete a model",
            "description": "Removes a model from a project: its file in src/models/ and its two lines in src/models/mod.rs. Refuses, changing nothing, while other code still uses it (such as a model with a foreign key to it): delete those first. Afterwards, makemigrations and migrate drop its table and every row in it, so confirm with the user before running them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project,
                    "name": { "type": "string", "description": "Model name in PascalCase, as it was generated, e.g. `BlogPost`." },
                },
                "required": ["project", "name"],
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "run",
            "title": "Run a project command",
            "description": "Runs `cargo run -- <command>` in a project and returns its output. makemigrations writes a migration for model changes (never asking questions: nothing is taken as renamed); migrate applies migrations; showmigrations lists them; sqlmigrate <name> prints a migration's SQL; check validates the configuration; routes lists the URLs; openapi prints the API description; apikey --username U --name N makes an API key. The first run compiles the project, which takes a few minutes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project,
                    "command": { "type": "string", "enum": ["makemigrations", "migrate", "showmigrations", "sqlmigrate", "check", "routes", "openapi", "apikey"] },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments after the command, e.g. [\"0001\"] for migrate or sqlmigrate." },
                },
                "required": ["project", "command"],
            },
        },
        {
            "name": "create_superuser",
            "title": "Create a superuser",
            "description": "Creates an account that can do everything in the project's admin, for logging in during development. Run migrate first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project,
                    "username": { "type": "string" },
                    "password": { "type": "string", "description": "At least 8 characters." },
                    "email": { "type": "string", "description": "Optional e-mail address; the account can log in with it too." },
                },
                "required": ["project", "username", "password"],
            },
        },
    ])
}

/// A tool's answer: text, marked as an error or not.
fn text(text: impl Into<String>, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": is_error })
}

fn call_tool(params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call needs a `name`".to_owned()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let string = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    let required = |key: &str| string(key).ok_or_else(|| format!("`{key}` is required"));
    let strings = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let outcome: Result<String, String> = match name {
        "docs_list" => Ok(docs_list()),
        "docs_read" => required("page").and_then(|page| docs_read(&page)),
        "docs_search" => required("query").map(|query| {
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(5, |n| usize::try_from(n.clamp(1, 20)).unwrap_or(5));
            docs_search(&query, limit)
        }),
        "new_project" => (|| {
            let directory = folder(&required("directory")?)?;
            let name = required("name")?;
            let database = match string("database").as_deref() {
                None | Some("sqlite") => crate::Database::Sqlite,
                Some("postgres") => crate::Database::Postgres,
                Some("mysql") => crate::Database::Mysql,
                Some(other) => return Err(format!("unknown database `{other}`")),
            };
            let framework = string("framework_path").map(PathBuf::from);
            crate::new_project(&directory, &name, framework.as_deref(), database)
        })(),
        "generate_model" => (|| {
            let project = project(&required("project")?)?;
            crate::generate_model(&project, &required("name")?, &strings("fields"))
        })(),
        "delete_model" => (|| {
            let project = project(&required("project")?)?;
            crate::delete_model(&crate::plan_model_deletion(&project, &required("name")?)?)
        })(),
        "run" => (|| {
            let project = project(&required("project")?)?;
            let command = required("command")?;
            let allowed = [
                "makemigrations",
                "migrate",
                "showmigrations",
                "sqlmigrate",
                "check",
                "routes",
                "openapi",
                "apikey",
            ];
            if !allowed.contains(&command.as_str()) {
                return Err(format!(
                    "`{command}` is not a command this tool runs; use one of {}",
                    allowed.join(", ")
                ));
            }
            let mut arguments = vec![command.clone()];
            let extra = strings("args");
            // Questions would wait for an answer on standard input forever.
            if command == "makemigrations" && !extra.iter().any(|a| a == "--no-input") {
                arguments.push("--no-input".into());
            }
            arguments.extend(extra);
            cargo_run(&project, &arguments, None)
        })(),
        "create_superuser" => (|| {
            let project = project(&required("project")?)?;
            let mut arguments = vec![
                "createsuperuser".to_owned(),
                "--username".to_owned(),
                required("username")?,
                "--password-stdin".to_owned(),
            ];
            if let Some(email) = string("email") {
                arguments.push("--email".into());
                arguments.push(email);
            }
            cargo_run(
                &project,
                &arguments,
                Some(format!("{}\n", required("password")?)),
            )
        })(),
        _ => return Err((-32602, format!("unknown tool `{name}`"))),
    };
    Ok(match outcome {
        Ok(report) => text(report, false),
        Err(message) => text(message, true),
    })
}

fn docs_list() -> String {
    let mut out = String::from(
        "Cadmus documentation. Read a page with docs_read, e.g. {\"page\": \"models\"}.\n",
    );
    let mut section = String::new();
    for page in docs::pages() {
        if page.section != section {
            section.clone_from(&page.section);
            out.push_str(&format!("\n{section}\n"));
        }
        out.push_str(&format!(
            "- {} ({}): {}\n",
            page.slug, page.title, page.summary
        ));
    }
    out
}

fn docs_read(slug: &str) -> Result<String, String> {
    match docs::page(slug) {
        Some(page) => Ok(format!("# {}\n\n{}", page.title, page.body)),
        None => Err(format!(
            "there is no page `{slug}`; the pages are: {}",
            docs::pages()
                .iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn docs_search(query: &str, limit: usize) -> String {
    let hits = docs::search(query, limit);
    if hits.is_empty() {
        return format!("nothing matches `{query}`; docs_list shows every page");
    }
    hits.iter()
        .map(|hit| {
            let mut text = hit.text.trim().to_owned();
            if text.len() > 2000 {
                let mut end = 2000;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("\n…");
            }
            format!("## {} (page `{}`)\n\n{text}", hit.heading, hit.page)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn resources() -> Value {
    let list: Vec<Value> = docs::pages()
        .iter()
        .map(|page| {
            json!({
                "uri": format!("cadmus://docs/{}", page.slug),
                "name": page.slug,
                "title": page.title,
                "description": page.summary,
                "mimeType": "text/markdown",
            })
        })
        .collect();
    json!({ "resources": list })
}

fn read_resource(params: &Value) -> Result<Value, (i64, String)> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or((-32602, "resources/read needs a `uri`".to_owned()))?;
    let page = uri
        .strip_prefix("cadmus://docs/")
        .and_then(docs::page)
        .ok_or((-32002, format!("no resource `{uri}`")))?;
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "text/markdown",
            "text": format!("# {}\n\n{}", page.title, page.body),
        }]
    }))
}

/// An existing folder, given as an absolute path.
fn folder(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!("`{}` is not an absolute path", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("there is no folder `{}`", path.display()));
    }
    Ok(path)
}

/// A Cadmus project folder.
fn project(path: &str) -> Result<PathBuf, String> {
    let path = folder(path)?;
    if !path.join("Cargo.toml").is_file() || !path.join("src/models/mod.rs").is_file() {
        return Err(format!(
            "`{}` is not a Cadmus project (no Cargo.toml and src/models/mod.rs); create one with new_project",
            path.display()
        ));
    }
    Ok(path)
}

/// The last `OUTPUT_LIMIT` bytes of a command's output.
fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_LIMIT {
        return text.to_owned();
    }
    let mut start = text.len() - OUTPUT_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Runs `cargo run --quiet -- <arguments>` in the project, feeding it `input`.
fn cargo_run(
    project: &Path,
    arguments: &[String],
    input: Option<String>,
) -> Result<String, String> {
    let mut child = Command::new("cargo")
        .args(["run", "--quiet", "--"])
        .args(arguments)
        .current_dir(project)
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run cargo (is Rust installed?): {e}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    // Read both streams as they come, so a full pipe never stalls the command.
    let read = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(status);
        }
        if started.elapsed() > COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let mut report = format!("$ cargo run -- {}\n", arguments.join(" "));
    if !stdout.trim().is_empty() {
        report.push_str(&tail(stdout.trim_end()));
        report.push('\n');
    }
    if !stderr.trim().is_empty() {
        report.push_str(&tail(stderr.trim_end()));
        report.push('\n');
    }
    match status {
        Some(status) if status.success() => Ok(report),
        Some(status) => Err(format!("{report}(failed: {status})")),
        None => Err(format!(
            "{report}(stopped after {} minutes)",
            COMMAND_TIMEOUT.as_secs() / 60
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        let line = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
        handle_line(&line.to_string()).expect("a reply")
    }

    fn tool(name: &str, arguments: Value) -> (bool, String) {
        let reply = call(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &reply["result"];
        (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_owned(),
        )
    }

    #[test]
    fn initialize_agrees_on_a_version_and_describes_the_server() {
        let reply = call(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }),
        );
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "cadmus");
        assert!(
            reply["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("makemigrations")
        );
        // An unknown version gets the newest.
        let reply = call("initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_an_error() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_line(&note.to_string()).is_none());
        assert_eq!(call("nonsense", json!({}))["error"]["code"], -32601);
        assert_eq!(handle_line("{not json").unwrap()["error"]["code"], -32700);
    }

    #[test]
    fn the_tools_are_listed_and_the_docs_can_be_read_and_searched() {
        let tools = call("tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "docs_list",
                "docs_read",
                "docs_search",
                "new_project",
                "generate_model",
                "delete_model",
                "run",
                "create_superuser"
            ]
        );
        let (error, list) = tool("docs_list", json!({}));
        assert!(!error && list.contains("- models (Models)"));
        let (error, page) = tool("docs_read", json!({ "page": "models" }));
        assert!(!error && page.starts_with("# Models"));
        let (error, missing) = tool("docs_read", json!({ "page": "nope" }));
        assert!(error && missing.contains("models"));
        let (error, hits) = tool("docs_search", json!({ "query": "list_filter" }));
        assert!(!error && hits.contains("list_filter"), "{hits}");

        let resources = call("resources/list", json!({}));
        let uri = resources["result"]["resources"][0]["uri"]
            .as_str()
            .unwrap()
            .to_owned();
        let read = call("resources/read", json!({ "uri": uri }));
        assert_eq!(read["result"]["contents"][0]["mimeType"], "text/markdown");
        assert_eq!(
            call("resources/read", json!({ "uri": "cadmus://docs/nope" }))["error"]["code"],
            -32002
        );
    }

    #[test]
    fn projects_and_models_are_created_through_tools() {
        let dir = std::env::temp_dir().join(format!("cadmus-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let directory = dir.display().to_string();

        let (error, report) = tool(
            "new_project",
            json!({ "directory": directory, "name": "shop" }),
        );
        assert!(!error, "{report}");
        assert!(report.contains("created"));
        let project = dir.join("shop").display().to_string();
        let (error, report) = tool(
            "generate_model",
            json!({ "project": project, "name": "Product", "fields": ["name:string", "price:float"] }),
        );
        assert!(!error, "{report}");
        assert!(dir.join("shop/src/models/product.rs").is_file());
        let (error, report) = tool(
            "delete_model",
            json!({ "project": project, "name": "Product" }),
        );
        assert!(!error, "{report}");
        assert!(report.contains("drops the table `product`"));
        assert!(!dir.join("shop/src/models/product.rs").exists());
        let (error, report) = tool(
            "delete_model",
            json!({ "project": project, "name": "Product" }),
        );
        assert!(error && report.contains("no model"));

        let (error, report) = tool(
            "generate_model",
            json!({ "project": "relative/path", "name": "X", "fields": [] }),
        );
        assert!(error && report.contains("absolute"));
        let (error, report) = tool("run", json!({ "project": project, "command": "serve" }));
        assert!(error && report.contains("not a command this tool runs"));
        let (error, report) = tool("run", json!({ "project": directory, "command": "check" }));
        assert!(error && report.contains("not a Cadmus project"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
