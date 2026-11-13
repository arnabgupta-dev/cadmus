//! The redesigned admin's behaviour (sorting, notices, a dashboard and forms that follow the
//! account's permissions) and the public home page.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::get;
use heraapi::{App, AuthConfig, HomeConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

mod note {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "note")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub rank: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<note::Entity>(ModelOptions::new().label("Notes"))
}

struct Site {
    app: Router,
    db: DatabaseConnection,
    token: String,
    root: Browser,
}

/// Notes "bravo" (rank 2), "alpha" (rank 3) and "charlie" (rank 1), in that id order.
async fn setup() -> Site {
    let db = common::database().await;
    let app = new_app(&db);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let token = token(&app, "root").await;
    for (title, rank) in [("bravo", 2), ("alpha", 3), ("charlie", 1)] {
        let (status, _) = api(&app, &token, "GET", "/api/note", None).await;
        assert_eq!(status, StatusCode::OK);
        let body = json!({ "title": title, "rank": rank });
        let (status, reply) = api(&app, &token, "POST", "/api/note", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }
    let root = Browser::login(&app, "root").await;
    Site {
        app,
        db,
        token,
        root,
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
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

async fn get_with(app: &Router, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::get(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn login(app: &Router, username: &str) -> Self {
        let request = Request::post("/admin/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username={username}&password={}",
                PASSWORD.replace(' ', "+")
            )))
            .unwrap();
        let reply = send(app, request).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let cookie = reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut browser = Self {
            cookie,
            csrf: String::new(),
        };
        let home = browser.get(app, "/admin/").await;
        let marker = "name=\"_csrf\" value=\"";
        let start = home.body.find(marker).unwrap() + marker.len();
        browser.csrf = home.body[start..].split('"').next().unwrap().to_owned();
        browser
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        get_with(app, path, &[("cookie", &self.cookie)]).await
    }

    async fn post(&self, app: &Router, path: &str, fields: &str) -> Reply {
        let request = Request::post(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("_csrf={}&{fields}", self.csrf)))
            .unwrap();
        send(app, request).await
    }
}

async fn token(app: &Router, username: &str) -> String {
    let body = json!({ "username": username, "password": PASSWORD });
    let (status, reply) = api(app, "", "POST", "/api/auth/token", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["token"].as_str().unwrap().to_owned()
}

async fn api(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let reply = send(app, request).await;
    (
        reply.status,
        serde_json::from_str(&reply.body).unwrap_or(Value::Null),
    )
}

/// The titles a list shows, in order.
fn titles(page: &str) -> Vec<String> {
    page.split("data-label=\"Title\">")
        .skip(1)
        .map(|cell| cell.split('<').next().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn lists_can_be_sorted_by_any_visible_column() {
    let site = setup().await;
    let (app, token) = (&site.app, &site.token);

    // The API: `sort=column`, `-` for highest first; ids in primary-key order by default.
    let order = |reply: Value, column: &str| -> Vec<Value> {
        reply["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[column].clone())
            .collect()
    };
    let (_, reply) = api(app, token, "GET", "/api/note", None).await;
    assert_eq!(
        order(reply, "title"),
        [json!("bravo"), json!("alpha"), json!("charlie")]
    );
    let (_, reply) = api(app, token, "GET", "/api/note?sort=title", None).await;
    assert_eq!(
        order(reply, "title"),
        [json!("alpha"), json!("bravo"), json!("charlie")]
    );
    let (_, reply) = api(app, token, "GET", "/api/note?sort=-rank", None).await;
    assert_eq!(order(reply, "rank"), [json!(3), json!(2), json!(1)]);
    // Sorting and paging work together: no row is skipped or repeated.
    let mut seen = Vec::new();
    for page in 1..=3 {
        let (_, reply) = api(
            app,
            token,
            "GET",
            &format!("/api/note?sort=-title&per_page=1&page={page}"),
            None,
        )
        .await;
        seen.extend(order(reply, "title"));
    }
    assert_eq!(seen, [json!("charlie"), json!("bravo"), json!("alpha")]);
    // An unknown column is refused, naming the parameter.
    let (status, reply) = api(app, token, "GET", "/api/note?sort=nope", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply["error"]["field"], "sort");

    // The admin: headings link to the next order, and say which one is in effect.
    let list = site.root.get(app, "/admin/note").await;
    assert_eq!(titles(&list.body), ["bravo", "alpha", "charlie"]);
    assert!(
        list.body.contains("href=\"&#x2f;admin/note?sort=title\""),
        "{}",
        list.body
    );
    assert!(!list.body.contains("aria-sort=\""));
    let list = site.root.get(app, "/admin/note?sort=title").await;
    assert_eq!(titles(&list.body), ["alpha", "bravo", "charlie"]);
    assert!(list.body.contains("aria-sort=\"ascending\""));
    assert!(
        list.body.contains("?sort=-title\""),
        "the next click reverses"
    );
    let list = site.root.get(app, "/admin/note?sort=-title").await;
    assert_eq!(titles(&list.body), ["charlie", "bravo", "alpha"]);
    assert!(list.body.contains("aria-sort=\"descending\""));
    // Searching keeps the order, and an unknown column is ignored rather than an error.
    let list = site.root.get(app, "/admin/note?sort=-title&q=a").await;
    assert_eq!(titles(&list.body), ["charlie", "bravo", "alpha"]);
    assert!(list.body.contains("name=\"sort\" value=\"-title\""));
    let list = site.root.get(app, "/admin/note?sort=nope").await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(titles(&list.body), ["bravo", "alpha", "charlie"]);
}

#[tokio::test]
async fn changes_are_confirmed_with_a_fixed_notice() {
    let site = setup().await;
    let (app, root) = (&site.app, &site.root);

    let reply = root
        .post(app, "/admin/note/new", "title=delta&rank=4")
        .await;
    assert_eq!(
        reply.headers[header::LOCATION],
        "/admin/note?notice=created"
    );
    let list = root.get(app, "/admin/note?notice=created").await;
    assert!(list.body.contains("role=\"status\"") && list.body.contains("Created."));
    let reply = root.post(app, "/admin/note/4", "title=delta&rank=5").await;
    assert_eq!(reply.headers[header::LOCATION], "/admin/note?notice=saved");
    let reply = root.post(app, "/admin/note/4/delete", "").await;
    assert_eq!(
        reply.headers[header::LOCATION],
        "/admin/note?notice=deleted"
    );
    assert!(
        root.get(app, "/admin/note?notice=deleted")
            .await
            .body
            .contains("Deleted.")
    );

    // Only known codes show anything: a crafted link cannot put words on the page.
    let list = root
        .get(app, "/admin/note?notice=Your+account+is+hacked")
        .await;
    assert!(!list.body.contains("role=\"status\"") && !list.body.contains("hacked"));

    let fields = format!(
        "username=sam&password={}&is_active=true&is_staff=true",
        PASSWORD.replace(' ', "+")
    );
    let reply = root.post(app, "/admin/auth/users/new", &fields).await;
    assert_eq!(
        reply.headers[header::LOCATION],
        "/admin/auth/users?notice=user_created"
    );
    assert!(
        root.get(app, "/admin/auth/users?notice=user_created")
            .await
            .body
            .contains("Account created.")
    );
}

#[tokio::test]
async fn the_admin_shows_each_account_exactly_what_it_may_do() {
    let site = setup().await;
    let (app, root) = (&site.app, &site.root);

    // A superuser: counts, full access, accounts, and a nudge towards two-factor.
    let home = root.get(app, "/admin/").await;
    assert!(home.body.contains(">3<small>rows</small>"), "{}", home.body);
    assert!(home.body.contains("Full access") && home.body.contains("admin/auth/users"));
    assert!(home.body.contains("Superuser") && home.body.contains("Two-factor authentication"));
    assert!(home.body.contains("admin/note/new"));

    // Someone who may only view notes.
    let fields = format!(
        "username=viewer&password={}&is_active=true&is_staff=true&perm=note:view",
        PASSWORD.replace(' ', "+")
    );
    assert_eq!(
        root.post(app, "/admin/auth/users/new", &fields)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let viewer = Browser::login(app, "viewer").await;
    let home = viewer.get(app, "/admin/").await;
    assert!(!home.body.contains("Full access") && !home.body.contains("admin/auth/users"));
    assert!(home.body.contains(">Staff<"));
    // Each action is spelled out, not shown by colour alone.
    for (action, allowed) in [
        ("View", true),
        ("Add", false),
        ("Change", false),
        ("Delete", false),
    ] {
        let marker = format!(
            "{action}<span class=\"sr-only\">: {}</span>",
            if allowed { "allowed" } else { "not allowed" }
        );
        assert!(home.body.contains(&marker), "{marker}");
    }
    assert!(!home.body.contains("admin/note/new"));
    // The navigation marks the model as view-only, and so does its list.
    assert!(home.body.contains("aria-label=\"View only\""));
    let list = viewer.get(app, "/admin/note").await;
    assert!(list.body.contains("View only") && !list.body.contains("admin/note/new"));
    // The record opens with every field read-only and nothing to press.
    let form = viewer.get(app, "/admin/note/1").await;
    assert!(form.body.contains("your account cannot change it"));
    assert!(form.body.contains("name=\"title\" value=\"bravo\""));
    let inputs = form.body.matches("<input id=\"f-").count();
    assert_eq!(
        inputs,
        form.body.matches(" readonly>").count(),
        "{}",
        form.body
    );
    assert!(!form.body.contains(">Save</button>") && !form.body.contains("confirm-delete"));

    // Someone who may edit but not delete sees Save and no Delete.
    let fields = format!(
        "username=editor&password={}&is_active=true&is_staff=true&perm=note:change",
        PASSWORD.replace(' ', "+")
    );
    assert_eq!(
        root.post(app, "/admin/auth/users/new", &fields)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let editor = Browser::login(app, "editor").await;
    let form = editor.get(app, "/admin/note/1").await;
    assert!(form.body.contains(">Save</button>") && !form.body.contains("confirm-delete"));
    assert!(!form.body.contains(" readonly>") || form.body.contains("name=\"id\""));
    // Root may delete, behind a confirmation.
    let form = root.get(app, "/admin/note/1").await;
    assert!(
        form.body.contains("data-confirm=\"confirm-delete\"")
            && form.body.contains("<dialog id=\"confirm-delete\"")
    );
}

#[tokio::test]
async fn the_home_page_shows_the_status_and_links_nowhere() {
    let site = setup().await;
    let app = &site.app;

    let page = get_with(app, "/", &[("accept", "text/html")]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(page.body.contains("All systems operational") && page.body.contains("Connected"));
    assert!(page.body.contains("<h1 class=\"name\">Administration</h1>"));
    // Nothing points at the admin or the documentation.
    for hidden in ["/admin", "/api/docs", "/api/redoc", "openapi"] {
        assert!(!page.body.contains(hidden), "{hidden}");
    }
    assert!(!page.body.contains("<a "), "no links at all");
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(page.headers[header::X_FRAME_OPTIONS], "DENY");

    // Monitors ask for JSON at the same URL.
    let reply = get_with(app, "/", &[("accept", "application/json")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    let status: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(status["status"], "operational");
    assert_eq!(status["database"]["status"], "connected");
    assert!(status["database"]["latency_ms"].is_number() && status["uptime_secs"].is_number());
    assert!(status["checked_at"].as_str().unwrap().ends_with('Z'));

    // Other methods and paths are not the home page.
    let request = Request::post("/").body(Body::empty()).unwrap();
    assert_eq!(
        send(app, request).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        get_with(app, "/nowhere", &[]).await.status,
        StatusCode::NOT_FOUND
    );

    // Title and tagline can be set.
    let app = new_app(&site.db)
        .home(
            HomeConfig::new()
                .title("Acme <Orders>")
                .tagline("Order processing"),
        )
        .router()
        .unwrap();
    let page = get_with(&app, "/", &[]).await;
    assert!(page.body.contains("Acme &lt;Orders&gt;") && page.body.contains("Order processing"));
}

#[tokio::test]
async fn browsers_opening_api_urls_are_sent_home_and_programs_are_not() {
    let site = setup().await;
    let app = &site.app;
    let navigate = [
        ("sec-fetch-mode", "navigate"),
        ("accept", "text/html,application/xhtml+xml"),
    ];

    for path in ["/api", "/api/", "/api/note", "/api/note/1", "/api/nothing"] {
        let reply = get_with(app, path, &navigate).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(reply.headers[header::LOCATION], "/", "{path}");
    }
    // An older browser that does not say how the request was made, but asks for HTML.
    let reply = get_with(app, "/api/note", &[("accept", "text/html")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    // Real API requests are untouched: with a token, from a script, or from curl.
    let bearer = format!("Bearer {}", site.token);
    let reply = get_with(
        app,
        "/api/note",
        &[navigate[0], navigate[1], ("authorization", &bearer)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = get_with(
        app,
        "/api/note",
        &[("sec-fetch-mode", "cors"), ("accept", "application/json")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = get_with(app, "/api/note", &[("accept", "*/*")]).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let request = Request::post("/api/auth/token")
        .header("sec-fetch-mode", "navigate")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "root", "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    assert_eq!(send(app, request).await.status, StatusCode::OK);

    // The documentation pages are meant for browsers, and the admin is not under the API.
    assert_eq!(
        get_with(app, "/api/docs", &navigate).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get_with(app, "/api/redoc", &navigate).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get_with(app, "/api/openapi.json", &navigate).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get_with(app, "/admin/login", &navigate).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get_with(app, "/apiary", &navigate).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_application_can_keep_slash_for_itself() {
    let site = setup().await;

    // Its own `/` route wins over the home page.
    let app = new_app(&site.db)
        .routes(Router::new().route("/", get(|| async { "my landing page" })))
        .router()
        .unwrap();
    assert_eq!(get_with(&app, "/", &[]).await.body, "my landing page");
    // The rest still falls through as before.
    assert_eq!(
        get_with(&app, "/nowhere", &[]).await.status,
        StatusCode::NOT_FOUND
    );

    // Turned off: no page at `/`, and browsers reach the API's own answer.
    let app = new_app(&site.db)
        .home(HomeConfig::new().enabled(false))
        .router()
        .unwrap();
    assert_eq!(get_with(&app, "/", &[]).await.status, StatusCode::NOT_FOUND);
    let reply = get_with(&app, "/api/note", &[("sec-fetch-mode", "navigate")]).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    assert!(
        HomeConfig::from_lookup(|k| (k == "HOME_PAGE_ENABLED").then(|| "perhaps".to_owned()))
            .is_err()
    );
}
