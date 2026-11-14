//! API documentation: the generated OpenAPI document, the pages that show it, and the
//! optional password in front of them.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use heraapi::{App, AuthConfig, DocsConfig, ModelOptions, Storage, create_superuser};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

mod author {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "author")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// One column of each kind that matters to the document.
mod blog_post {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "blog_post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub subtitle: Option<String>,
        #[sea_orm(default_value = 0)]
        pub views: i32,
        pub rating: Option<f64>,
        pub published: bool,
        pub published_at: Option<DateTime>,
        pub day: Option<Date>,
        pub extra: Option<Json>,
        pub author_id: i32,
        pub internal_note: Option<String>,
        // Read-only in the API, so the database has to supply it.
        #[sea_orm(default_value = "untitled")]
        pub slug: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::author::Entity",
            from = "Column::AuthorId",
            to = "super::author::Column::Id"
        )]
        Author,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection) -> App {
    App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().login_limit(4, 120),
    )
    .model::<author::Entity>(ModelOptions::new().label("Authors"))
    .model::<blog_post::Entity>(
        ModelOptions::new()
            .label("Blog posts")
            .hidden_fields(["internal_note"])
            .readonly_fields(["slug"]),
    )
}

async fn database() -> DatabaseConnection {
    let db = common::database().await;
    new_app(&db).migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    db
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// `GET path`, optionally with HTTP Basic credentials.
async fn get(app: &Router, path: &str, basic: Option<(&str, &str)>) -> Reply {
    use base64::Engine as _;
    let mut request = Request::get(path);
    if let Some((username, password)) = basic {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        request = request.header(header::AUTHORIZATION, format!("Basic {encoded}"));
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

/// Every `$ref` in the document, with where it was found.
fn references(value: &Value, at: &str, found: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                match (key.as_str(), inner) {
                    ("$ref", Value::String(target)) => found.push((at.to_owned(), target.clone())),
                    _ => references(inner, &format!("{at}/{key}"), found),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|inner| references(inner, at, found)),
        _ => {}
    }
}

fn names(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_document_describes_every_model_as_the_api_behaves() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();
    let reply = get(&app, "/api/openapi.json", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "application/json");
    let doc = reply.json();

    assert_eq!(doc["openapi"], "3.0.3");
    assert_eq!(doc["info"]["title"], "Administration API");
    assert_eq!(doc["info"]["version"], "0.1.0");

    // It holds together: every reference points at something that exists.
    let mut found = Vec::new();
    references(&doc, "", &mut found);
    assert!(found.len() > 40, "{}", found.len());
    for (at, target) in &found {
        let pointer = target.strip_prefix('#').expect("local reference");
        assert!(
            doc.pointer(pointer).is_some(),
            "{at} refers to missing {target}"
        );
    }
    // Every required field is one of the properties, and operation ids are unique.
    let schemas = &doc["components"]["schemas"];
    for (name, schema) in schemas.as_object().unwrap() {
        for required in strings(&schema["required"]) {
            assert!(
                schema["properties"].get(required).is_some(),
                "{name}.{required}"
            );
        }
    }
    let mut ids = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            if let Some(id) = operation.get("operationId").and_then(Value::as_str) {
                assert!(
                    !ids.contains(&id.to_owned()),
                    "{method} {path}: duplicate {id}"
                );
                ids.push(id.to_owned());
                assert!(
                    operation["responses"]
                        .as_object()
                        .is_some_and(|r| !r.is_empty())
                );
            }
        }
    }

    // The paths of a model.
    let mut paths = names(&doc["paths"]);
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "/api/auth/token",
            "/api/author",
            "/api/author/{id}",
            "/api/blog_post",
            "/api/blog_post/{id}"
        ]
    );
    let mut methods = names(&doc["paths"]["/api/blog_post/{id}"]);
    methods.sort_unstable();
    assert_eq!(methods, ["delete", "get", "parameters", "patch", "put"]);
    // Columns, paths and operations come in the order they were declared, not alphabetically.
    assert_eq!(
        names(&doc["paths"]),
        [
            "/api/auth/token",
            "/api/author",
            "/api/author/{id}",
            "/api/blog_post",
            "/api/blog_post/{id}"
        ]
    );
    assert_eq!(
        names(&doc["paths"]["/api/blog_post/{id}"]),
        ["parameters", "get", "put", "patch", "delete"]
    );
    assert_eq!(
        names(&doc["components"]["schemas"]["BlogPost"]["properties"])[..4],
        ["id", "title", "subtitle", "views"]
    );
    assert_eq!(
        doc["paths"]["/api/blog_post/{id}"]["patch"]["summary"],
        "Change some fields of a blog post"
    );
    assert_eq!(
        doc["paths"]["/api/author"]["post"]["summary"],
        "Create an author"
    );
    let list = &doc["paths"]["/api/blog_post"]["get"];
    assert_eq!(list["tags"], json!(["Blog posts"]));
    assert_eq!(list["operationId"], "list_blog_post");
    assert!(
        list["description"]
            .as_str()
            .unwrap()
            .contains("`view` permission on `blog_post`")
    );
    let query: Vec<&str> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    // Then one filter per visible column; the hidden one cannot be filtered on.
    assert_eq!(query[..4], ["page", "per_page", "q", "sort"]);
    assert!(query.contains(&"published") && query.contains(&"author_id"));
    assert!(!query.contains(&"internal_note"));
    let published_at = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "published_at")
        .unwrap();
    assert!(
        published_at["description"]
            .as_str()
            .unwrap()
            .contains("published_at__range=today|7d|30d|month|year")
    );

    // What is returned: everything but the hidden column.
    let read = &schemas["BlogPost"]["properties"];
    assert!(read.get("internal_note").is_none());
    assert_eq!(read["id"], json!({ "type": "integer" }));
    assert_eq!(read["title"], json!({ "type": "string" }));
    assert_eq!(
        read["subtitle"],
        json!({ "type": "string", "nullable": true })
    );
    assert_eq!(
        read["rating"],
        json!({ "type": "number", "nullable": true })
    );
    assert_eq!(read["published"], json!({ "type": "boolean" }));
    assert_eq!(
        read["day"],
        json!({ "type": "string", "format": "date", "nullable": true })
    );
    assert_eq!(read["published_at"]["type"], "string");
    assert!(
        read["published_at"].get("format").is_none(),
        "plain timestamps have no zone"
    );
    assert!(read["extra"].get("type").is_none(), "any JSON value");
    assert_eq!(read["author_id"]["description"], "The `id` of a `author`");
    assert!(read.get("slug").is_some());

    // What may be sent: no generated key, no read-only or hidden column.
    let create = &schemas["BlogPostCreate"];
    for absent in ["id", "slug", "internal_note"] {
        assert!(create["properties"].get(absent).is_none(), "{absent}");
    }
    assert_eq!(create["additionalProperties"], false);
    // A new row may leave out what is nullable or has a database default...
    assert_eq!(
        strings(&create["required"]),
        ["title", "published", "author_id"]
    );
    // ...while replacing a row needs every column that cannot be null.
    assert_eq!(
        strings(&schemas["BlogPostReplace"]["required"]),
        ["title", "views", "published", "author_id"]
    );
    // A patch needs nothing in particular.
    assert!(schemas["BlogPostPatch"].get("required").is_none());
    assert_eq!(
        names(&schemas["BlogPostPatch"]["properties"]),
        names(&create["properties"])
    );
    // Where creating and replacing take the same body, it is described once.
    assert!(schemas.get("AuthorInput").is_some() && schemas.get("AuthorCreate").is_none());
    assert_eq!(
        doc["paths"]["/api/author"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/AuthorInput"
    );
    assert_eq!(
        doc["paths"]["/api/author/{id}"]["put"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/AuthorInput"
    );

    // Logging in needs no token; everything else takes one, any way of supplying it.
    assert_eq!(
        doc["paths"]["/api/auth/token"]["post"]["security"],
        json!([])
    );
    assert_eq!(
        doc["security"],
        json!([{ "password": [] }, { "bearer": [] }, { "apiKey": [] }])
    );
    let schemes = &doc["components"]["securitySchemes"];
    assert_eq!(
        schemes["password"]["flows"]["password"]["tokenUrl"],
        "/api/auth/token"
    );
    assert_eq!(schemes["bearer"]["scheme"], "bearer");

    // Files appear only when uploads are enabled.
    assert!(doc["paths"].get("/api/files").is_none());
    let with_files = new_app(&db).storage(Storage::memory()).openapi().unwrap();
    for path in [
        "/api/files",
        "/api/files/{id}",
        "/api/files/{id}/content",
        "/api/uploads",
        "/api/uploads/{id}",
    ] {
        assert!(with_files["paths"].get(path).is_some(), "{path}");
    }
    let mut found = Vec::new();
    references(&with_files, "", &mut found);
    for (at, target) in &found {
        assert!(
            with_files.pointer(&target[1..]).is_some(),
            "{at} refers to missing {target}"
        );
    }
    // What the command prints is what is served.
    assert_eq!(new_app(&db).openapi().unwrap(), doc);
}

#[tokio::test]
async fn the_document_matches_what_the_api_accepts() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();
    let doc = get(&app, "/api/openapi.json", None).await.json();

    // The login dialog of the documentation sends a form and reads `access_token`.
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "grant_type=password&username=root&password={}&scope=&client_id=&client_secret=",
            PASSWORD.replace(' ', "+")
        )))
        .unwrap();
    let reply = send(&app, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let token = reply.json()["access_token"].as_str().unwrap().to_owned();
    assert_eq!(reply.json()["token"], token.as_str());
    assert_eq!(reply.json()["token_type"], "Bearer");
    // A wrong password and a form without a password are refused as usual.
    let request = Request::post("/api/auth/token")
        .header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(Body::from("username=root&password=wrong"))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::UNAUTHORIZED);
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("username=root"))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::BAD_REQUEST);

    let call = |method: &str, path: &str, body: Value| {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(&app, request)
    };
    assert_eq!(
        call("POST", "/api/author", json!({ "name": "Ada" }))
            .await
            .status,
        StatusCode::CREATED
    );

    // A body with exactly the fields the document requires is accepted, and what comes back
    // has exactly the properties the document lists.
    let schemas = &doc["components"]["schemas"];
    let minimal = json!({ "title": "Hello", "published": true, "author_id": 1 });
    assert_eq!(
        names(&minimal),
        strings(&schemas["BlogPostCreate"]["required"])
    );
    let reply = call("POST", "/api/blog_post", minimal.clone()).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let mut returned = names(&reply.json())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut documented = names(&schemas["BlogPost"]["properties"])
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    returned.sort_unstable();
    documented.sort_unstable();
    assert_eq!(returned, documented);

    // Leaving out any one required field is refused, naming it.
    for missing in strings(&schemas["BlogPostCreate"]["required"]) {
        let mut body = minimal.clone();
        body.as_object_mut().unwrap().remove(missing);
        let reply = call("POST", "/api/blog_post", body).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{missing}");
        assert_eq!(reply.json()["error"]["field"], missing);
    }
    // Replacing needs `views` as well, as documented.
    let reply = call("PUT", "/api/blog_post/1", minimal.clone()).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.json()["error"]["field"], "views");
    let mut full = minimal.clone();
    full["views"] = json!(3);
    assert_eq!(
        call("PUT", "/api/blog_post/1", full).await.status,
        StatusCode::OK
    );
    // Fields the write schemas leave out are refused: `additionalProperties: false`.
    for extra in ["id", "slug", "internal_note", "nonsense"] {
        let reply = call("PATCH", "/api/blog_post/1", json!({ extra: "x" })).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{extra}");
    }
    assert_eq!(
        call("PATCH", "/api/blog_post/1", json!({})).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_pages_load_the_viewers_and_point_them_at_the_document() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();

    let page = get(&app, "/api/docs", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(page.body.contains("<title>Administration API</title>"));
    assert!(
        page.body
            .contains("swagger-ui-dist@5.17.14&#x2f;swagger-ui-bundle.js"),
        "{}",
        page.body
    );
    assert!(page.body.contains("swagger-ui.css") && page.body.contains("SwaggerUIBundle({"));
    assert!(
        page.body
            .contains("data-url=\"&#x2f;api&#x2f;openapi.json\"")
    );

    let page = get(&app, "/api/redoc", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("<redoc spec-url=\"&#x2f;api&#x2f;openapi.json\">")
    );
    assert!(page.body.contains("redoc.standalone.js"));

    // Title, version, introduction, self-hosted viewers and extra paths can all be set.
    let docs = DocsConfig::new()
        .title("Shop <API>")
        .version("2.5.0")
        .description("Everything about the **shop**.")
        .assets("/static/swagger.js", "/static/swagger.css", "/static/redoc.js")
        .extend(|document| {
            document["paths"]["/health"] = json!({ "get": { "summary": "Health check", "security": [], "responses": { "200": { "description": "ok" } } } });
        });
    let app = new_app(&db).docs(docs).router().unwrap();
    let doc = get(&app, "/api/openapi.json", None).await.json();
    assert_eq!(doc["info"]["title"], "Shop <API>");
    assert_eq!(doc["info"]["version"], "2.5.0");
    assert!(
        doc["info"]["description"]
            .as_str()
            .unwrap()
            .starts_with("Everything about the **shop**.\n\n")
    );
    assert_eq!(doc["paths"]["/health"]["get"]["summary"], "Health check");
    let page = get(&app, "/api/docs", None).await;
    assert!(
        page.body.contains("<title>Shop &lt;API&gt;</title>"),
        "escaped: {}",
        page.body
    );
    assert!(
        page.body.contains("src=\"&#x2f;static&#x2f;swagger.js\"")
            && !page.body.contains("jsdelivr")
    );
    assert!(
        get(&app, "/api/redoc", None)
            .await
            .body
            .contains("&#x2f;static&#x2f;redoc.js")
    );

    // The routes command and the model names stay clear of the documentation's URLs.
    for reserved in ["docs", "redoc"] {
        assert!(doc["paths"].get(format!("/api/{reserved}")).is_none());
    }
}

#[tokio::test]
async fn a_password_can_be_put_in_front_of_the_documentation() {
    let db = database().await;
    let app = new_app(&db)
        .docs(DocsConfig::new().protect("reader", "open sesame"))
        .router()
        .unwrap();

    for path in ["/api/docs", "/api/redoc", "/api/openapi.json"] {
        // Nothing without credentials, and the browser is told to ask for them.
        let reply = get(&app, path, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        let challenge = reply.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(challenge.starts_with("Basic realm="), "{challenge}");
        assert!(!reply.body.contains("swagger") && !reply.body.contains("openapi"));
        // The right ones open it.
        let reply = get(&app, path, Some(("reader", "open sesame"))).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
    }
    // An account's login is not the documentation's password, and the other way round.
    assert_eq!(
        get(&app, "/api/docs", Some(("root", PASSWORD)))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "reader", "password": "open sesame" }).to_string(),
        ))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::UNAUTHORIZED);
    // The API itself is unaffected.
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "root", "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::OK);

    // Guessing is limited like logins are (4 attempts here: the one above and three more),
    // and a correct entry resets the count.
    for wrong in [
        ("reader", "x"),
        ("x", "open sesame"),
        ("reader", "open sesame "),
    ] {
        assert_eq!(
            get(&app, "/api/docs", Some(wrong)).await.status,
            StatusCode::UNAUTHORIZED,
            "{wrong:?}"
        );
    }
    let locked = get(&app, "/api/docs", Some(("reader", "open sesame"))).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.headers.contains_key(header::RETRY_AFTER));
    // Malformed credentials are simply not credentials.
    let request = Request::get("/api/docs")
        .header(header::AUTHORIZATION, "Basic not-base64!")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::UNAUTHORIZED);

    // Set from the environment: both or neither.
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    assert!(DocsConfig::from_lookup(lookup(&[("DOCS_USERNAME", "reader")])).is_err());
    assert!(DocsConfig::from_lookup(lookup(&[("DOCS_PASSWORD", "secret")])).is_err());
    assert!(DocsConfig::from_lookup(lookup(&[("DOCS_ENABLED", "maybe")])).is_err());
    let config = DocsConfig::from_lookup(lookup(&[
        ("DOCS_USERNAME", "reader"),
        ("DOCS_PASSWORD", "from the env"),
    ]))
    .unwrap();
    // The password never shows up when the settings are printed.
    assert!(!format!("{config:?}").contains("from the env"));
    let app = new_app(&db).docs(config).router().unwrap();
    assert_eq!(
        get(&app, "/api/docs", None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&app, "/api/docs", Some(("reader", "from the env")))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_documentation_can_be_turned_off() {
    let db = database().await;
    let app = new_app(&db).docs(DocsConfig::new().enabled(false));
    assert!(app.openapi().is_err());
    let app = app.router().unwrap();
    for path in ["/api/docs", "/api/redoc", "/api/openapi.json"] {
        let reply = get(&app, path, None).await;
        // Nothing is mounted there: the model routes answer as for any unknown name.
        assert!(
            matches!(
                reply.status,
                StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND
            ),
            "{path}: {}",
            reply.status
        );
        assert!(!reply.body.contains("swagger") && !reply.body.contains("\"openapi\""));
    }
}
