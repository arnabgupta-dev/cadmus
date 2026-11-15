//! Rich-text fields: an editor in the admin, HTML in the API, and cleaning of whatever is
//! written, however it is written.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use cadmus::{App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::{ConnectionTrait, DatabaseConnection};
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

mod post {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        #[sea_orm(column_type = "Text")]
        pub body: String,
        pub views: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
const DIRTY: &str = r#"<h2>Hello</h2><p onclick="steal()">A <b>bold</b> <a href="javascript:x()">move</a></p><script>alert(1)</script><img src="x" onerror="alert(2)">"#;
const CLEAN: &str =
    r#"<h2>Hello</h2><p>A <b>bold</b> <a rel="noopener noreferrer">move</a></p><img src="x">"#;

fn new_app(db: &DatabaseConnection, options: ModelOptions) -> App {
    App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().cookie_secure(false),
    )
    .model::<post::Entity>(options)
}

async fn site() -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = new_app(&db, ModelOptions::new().rich_text(["body"]));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    (app.router().unwrap(), db)
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
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn token(app: &Router) -> String {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "root", "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    let reply = send(app, request).await;
    format!("Bearer {}", reply.json()["token"].as_str().unwrap())
}

async fn login(app: &Router) -> (String, String) {
    let request = Request::post("/admin/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "username=root&password={}",
            PASSWORD.replace(' ', "+")
        )))
        .unwrap();
    let reply = send(app, request).await;
    let cookie = reply.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let page = get(app, "/admin/post/new", &cookie).await;
    let marker = "name=\"_csrf\" value=\"";
    let start = page.body.find(marker).unwrap() + marker.len();
    let csrf = page.body[start..].split('"').next().unwrap().to_owned();
    (cookie, csrf)
}

async fn get(app: &Router, path: &str, cookie: &str) -> Reply {
    let request = Request::get(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

#[tokio::test]
async fn html_from_the_api_is_cleaned_before_it_is_stored() {
    let (app, db) = site().await;
    let bearer = token(&app).await;
    let request = Request::post("/api/post")
        .header(header::AUTHORIZATION, &bearer)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "title": "First", "body": DIRTY, "views": 1 }).to_string(),
        ))
        .unwrap();
    let created = send(&app, request).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.json()["body"], CLEAN);

    let stored = db
        .query_one_raw(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT body FROM post",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.try_get::<String>("", "body").unwrap(), CLEAN);

    // A partial update is cleaned too.
    let request = Request::patch("/api/post/1")
        .header(header::AUTHORIZATION, &bearer)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "body": "<p style=\"x\">Plain</p><iframe></iframe>" }).to_string(),
        ))
        .unwrap();
    let patched = send(&app, request).await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    assert_eq!(patched.json()["body"], "<p>Plain</p>");

    // The API documentation says what the field holds.
    let docs = send(
        &app,
        Request::get("/api/openapi.json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let schema = &docs.json()["components"]["schemas"];
    let body = schema
        .as_object()
        .unwrap()
        .values()
        .find_map(|s| s["properties"].get("body"))
        .expect("a schema with the body field");
    assert_eq!(body["contentMediaType"], "text/html");
}

#[tokio::test]
async fn the_admin_edits_rich_text_with_the_editor() {
    let (app, db) = site().await;
    let (cookie, csrf) = login(&app).await;

    let page = get(&app, "/admin/post/new", &cookie).await;
    assert!(page.body.contains("data-rich-text"), "{}", page.body);
    assert!(
        page.body
            .contains("&#x2f;admin/static/quill-2.0.3/quill.js")
    );
    assert!(
        page.body
            .contains("&#x2f;admin/static/quill-2.0.3/quill.snow.css")
    );
    let policy = page.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    let nonce = policy
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    assert!(page.body.contains(&format!(
        "<script nonce=\"{nonce}\" src=\"&#x2f;admin/static/quill-2.0.3/quill.js\">"
    )));

    // A form post goes through the same cleaning.
    let body = format!(
        "_csrf={csrf}&title=From+the+form&views=2&body={}",
        url::form_urlencoded::byte_serialize(DIRTY.as_bytes()).collect::<String>()
    );
    let request = Request::post("/admin/post/new")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let saved = send(&app, request).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);

    // HTML that reached the database some other way is cleaned before the editor sees it.
    db.execute_unprepared(&format!(
        "UPDATE post SET body = '{}' WHERE id = 1",
        DIRTY.replace('\'', "''")
    ))
    .await
    .unwrap();
    let form = get(&app, "/admin/post/1", &cookie).await;
    assert!(!form.body.contains("onerror"), "{}", form.body);
    assert!(!form.body.contains("alert(1)"));
    assert!(
        form.body.contains("&lt;h2&gt;Hello&lt;&#x2f;h2&gt;"),
        "escaped inside the textarea"
    );

    // Lists show the text, not the markup.
    let list = get(&app, "/admin/post", &cookie).await;
    assert!(list.body.contains("Hello A bold move"), "{}", list.body);
    assert!(!list.body.contains("&lt;h2&gt;"));

    // Pages without rich text do not load the editor.
    let index = get(&app, "/admin/", &cookie).await;
    assert!(!index.body.contains("quill.js"));
}

#[tokio::test]
async fn the_editor_files_are_served_cached_and_compressed() {
    let (app, _db) = site().await;
    let request = Request::get("/admin/static/quill-2.0.3/quill.js")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let reply = app.clone().oneshot(request).await.unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let headers = reply.headers();
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");

    let request = Request::get("/admin/static/quill-2.0.3/quill.snow.css")
        .body(Body::empty())
        .unwrap();
    let reply = send(&app, request).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains(".ql-snow"));
}

#[tokio::test]
async fn rich_text_must_name_a_text_column() {
    let db = common::database().await;
    for column in ["views", "nothing"] {
        let error = new_app(&db, ModelOptions::new().rich_text([column]))
            .router()
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains(&format!("rich-text field `{column}`")),
            "{error}"
        );
    }
}
