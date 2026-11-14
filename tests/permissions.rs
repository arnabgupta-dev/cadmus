//! Permissions: superusers, staff with per-model permissions, groups, and the Users and
//! Groups pages that manage them.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use herapi::{
    AdminUser, App, AuthConfig, EnvUserStore, ModelOptions, Storage, create_superuser,
    hash_password, promote,
};
use http_body_util::BodyExt;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend};
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
        pub text: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod task {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "task")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
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
        .model::<task::Entity>(ModelOptions::new().label("Tasks"))
        .storage(Storage::memory())
}

/// An app with two models, uploads, a superuser `root` and one note and one task.
async fn setup() -> (Router, DatabaseConnection, Browser) {
    let db = common::database().await;
    let app = new_app(&db);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let root = Browser::login(&app, "root", PASSWORD).await;
    let token = token(&app, "root", PASSWORD).await.unwrap();
    for (path, body) in [
        ("/api/note", json!({ "text": "first note" })),
        ("/api/task", json!({ "title": "first task" })),
    ] {
        let (status, reply) = api(&app, &token, "POST", path, Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }
    (app, db, root)
}

struct Reply {
    status: StatusCode,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> (Reply, Option<String>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    (Reply { status, body }, cookie)
}

/// A logged-in admin session: the cookie and the CSRF token its forms carry.
struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn try_login(app: &Router, username: &str, password: &str) -> Option<Self> {
        let request = Request::post("/admin/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username={username}&password={}",
                password.replace(' ', "+")
            )))
            .unwrap();
        let (reply, cookie) = send(app, request).await;
        if reply.status != StatusCode::SEE_OTHER {
            return None;
        }
        let mut browser = Self {
            cookie: cookie.unwrap(),
            csrf: String::new(),
        };
        // Every page carries the token in its log-out form.
        let home = browser.get(app, "/admin/").await;
        let marker = "name=\"_csrf\" value=\"";
        let start = home.body.find(marker).expect("a CSRF token") + marker.len();
        browser.csrf = home.body[start..].split('"').next().unwrap().to_owned();
        Some(browser)
    }

    async fn login(app: &Router, username: &str, password: &str) -> Self {
        Self::try_login(app, username, password)
            .await
            .expect("login succeeds")
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        let request = Request::get(path)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        send(app, request).await.0
    }

    /// Submits a form; `fields` is the URL-encoded body without the CSRF token.
    async fn post(&self, app: &Router, path: &str, fields: &str) -> Reply {
        let request = Request::post(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("_csrf={}&{fields}", self.csrf)))
            .unwrap();
        send(app, request).await.0
    }

    /// Creates an active staff account through the Users page.
    async fn add_user(&self, app: &Router, username: &str, extra: &str) {
        let fields = format!(
            "username={username}&password={}&is_active=true&is_staff=true&{extra}",
            PASSWORD.replace(' ', "+")
        );
        let reply = self.post(app, "/admin/auth/users/new", &fields).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    }
}

async fn token(app: &Router, username: &str, password: &str) -> Option<String> {
    let body = json!({ "username": username, "password": password });
    let (status, reply) = api(app, "", "POST", "/api/auth/token", Some(body)).await;
    (status == StatusCode::OK).then(|| reply["token"].as_str().unwrap().to_owned())
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
    let (reply, _) = send(app, request).await;
    (
        reply.status,
        serde_json::from_str(&reply.body).unwrap_or(Value::Null),
    )
}

/// The status of an API call, with the error code checked when it is a refusal.
async fn status(app: &Router, token: &str, method: &str, path: &str, body: Option<Value>) -> u16 {
    let (status, reply) = api(app, token, method, path, body).await;
    if status == StatusCode::FORBIDDEN {
        assert_eq!(reply["error"]["code"], "forbidden", "{reply}");
    }
    status.as_u16()
}

/// The id in the `Edit` link of the row that shows `name` on a Users or Groups page.
fn id_of(page: &str, kind: &str, name: &str) -> String {
    let row = page
        .split("<tr>")
        .find(|row| row.contains(&format!("<strong>{name}</strong>")))
        .unwrap_or_else(|| panic!("no row for {name}"));
    // Templates HTML-escape the leading slash of the base path, so match what follows it.
    let marker = format!("admin/auth/{kind}/");
    let start = row.find(&marker).unwrap() + marker.len();
    row[start..].split('"').next().unwrap().to_owned()
}

#[tokio::test]
async fn staff_without_permissions_can_log_in_but_do_nothing() {
    let (app, _db, root) = setup().await;
    root.add_user(&app, "sam", "").await;

    let sam = token(&app, "sam", PASSWORD).await.expect("sam can log in");
    for (method, path, body) in [
        ("GET", "/api/note", None),
        ("GET", "/api/note/1", None),
        ("POST", "/api/note", Some(json!({ "text": "x" }))),
        ("PATCH", "/api/note/1", Some(json!({ "text": "x" }))),
        ("PUT", "/api/note/1", Some(json!({ "text": "x" }))),
        ("DELETE", "/api/note/1", None),
        ("GET", "/api/files", None),
    ] {
        assert_eq!(
            status(&app, &sam, method, path, body).await,
            403,
            "{method} {path}"
        );
    }
    // Nothing was changed by the refused writes.
    let root_token = token(&app, "root", PASSWORD).await.unwrap();
    let (_, list) = api(&app, &root_token, "GET", "/api/note", None).await;
    assert_eq!(list["meta"]["total"], 1);
    assert_eq!(list["data"][0]["text"], "first note");

    let sam = Browser::login(&app, "sam", PASSWORD).await;
    let home = sam.get(&app, "/admin/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        !home.body.contains("Notes") && !home.body.contains("Tasks"),
        "{}",
        home.body
    );
    assert!(!home.body.contains("/auth/users"));
    for path in [
        "/admin/note",
        "/admin/note/1",
        "/admin/note/new",
        "/admin/auth/users",
    ] {
        let page = sam.get(&app, path).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{path}");
        assert!(
            page.body.contains("<main id=\"main\""),
            "a page, not bare text"
        );
    }
}

#[tokio::test]
async fn permissions_are_enforced_per_model_and_action() {
    let (app, _db, root) = setup().await;
    root.add_user(
        &app,
        "sam",
        "perm=note:view&perm=task:change&perm=task:add&perm=nonsense:view&perm=note:fly",
    )
    .await;
    let sam = token(&app, "sam", PASSWORD).await.unwrap();

    // Notes: view only.
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 200);
    assert_eq!(status(&app, &sam, "GET", "/api/note/1", None).await, 200);
    assert_eq!(
        status(
            &app,
            &sam,
            "POST",
            "/api/note",
            Some(json!({ "text": "x" }))
        )
        .await,
        403
    );
    assert_eq!(
        status(
            &app,
            &sam,
            "PATCH",
            "/api/note/1",
            Some(json!({ "text": "x" }))
        )
        .await,
        403
    );
    assert_eq!(status(&app, &sam, "DELETE", "/api/note/1", None).await, 403);

    // Tasks: add and change, and therefore view; not delete.
    assert_eq!(status(&app, &sam, "GET", "/api/task", None).await, 200);
    assert_eq!(
        status(
            &app,
            &sam,
            "POST",
            "/api/task",
            Some(json!({ "title": "second" }))
        )
        .await,
        201
    );
    assert_eq!(
        status(
            &app,
            &sam,
            "PATCH",
            "/api/task/1",
            Some(json!({ "title": "renamed" }))
        )
        .await,
        200
    );
    assert_eq!(status(&app, &sam, "DELETE", "/api/task/1", None).await, 403);

    // An unknown model is still "not found", not "forbidden".
    assert_eq!(status(&app, &sam, "GET", "/api/nothing", None).await, 404);

    // The admin shows only what can be done.
    let sam = Browser::login(&app, "sam", PASSWORD).await;
    let home = sam.get(&app, "/admin/").await;
    assert!(home.body.contains("Notes") && home.body.contains("Tasks"));
    assert!(
        !home.body.contains("admin/note/new"),
        "no Add link for notes"
    );
    assert!(home.body.contains("admin/task/new"));

    let notes = sam.get(&app, "/admin/note").await;
    assert_eq!(notes.status, StatusCode::OK);
    assert!(notes.body.contains("first note"));
    assert!(notes.body.contains(">View</a>") && !notes.body.contains(">Edit</a>"));
    assert!(!notes.body.contains("admin/note/new"));

    let note = sam.get(&app, "/admin/note/1").await;
    assert_eq!(note.status, StatusCode::OK);
    assert!(!note.body.contains(">Save</button>") && !note.body.contains(">Delete</button>"));
    assert_eq!(
        sam.get(&app, "/admin/note/new").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/note/new", "text=sneaky")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/note/1", "text=sneaky").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/note/1/delete", "").await.status,
        StatusCode::FORBIDDEN
    );

    let task = sam.get(&app, "/admin/task/1").await;
    assert!(task.body.contains(">Save</button>") && !task.body.contains(">Delete</button>"));
    assert_eq!(
        sam.post(&app, "/admin/task/1", "title=from+the+form")
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        sam.post(&app, "/admin/task/1/delete", "").await.status,
        StatusCode::FORBIDDEN
    );

    let notes = root.get(&app, "/admin/note").await;
    assert!(notes.body.contains("first note") && !notes.body.contains("sneaky"));
    assert!(
        root.get(&app, "/admin/task")
            .await
            .body
            .contains("from the form")
    );
}

#[tokio::test]
async fn group_permissions_add_to_the_accounts_own() {
    let (app, _db, root) = setup().await;
    let reply = root
        .post(
            &app,
            "/admin/auth/groups/new",
            "name=Editors&perm=note:add&perm=note:delete",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let groups = root.get(&app, "/admin/auth/groups").await;
    let group = id_of(&groups.body, "groups", "Editors");

    root.add_user(&app, "sam", &format!("perm=note:view&group={group}"))
        .await;
    let sam = token(&app, "sam", PASSWORD).await.unwrap();
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 200);
    assert_eq!(
        status(
            &app,
            &sam,
            "POST",
            "/api/note",
            Some(json!({ "text": "x" }))
        )
        .await,
        201
    );
    assert_eq!(
        status(
            &app,
            &sam,
            "PATCH",
            "/api/note/1",
            Some(json!({ "text": "x" }))
        )
        .await,
        403
    );

    // The forms show what is stored: the group's boxes, and the account's own, not the sum.
    let form = root.get(&app, &format!("/admin/auth/groups/{group}")).await;
    assert!(
        form.body
            .contains("value=\"note:add\" aria-label=\"Notes: add\" checked")
    );
    assert!(
        form.body
            .contains("value=\"note:view\" aria-label=\"Notes: view\">")
    );
    let users = root.get(&app, "/admin/auth/users").await;
    let sam_id = id_of(&users.body, "users", "sam");
    let form = root.get(&app, &format!("/admin/auth/users/{sam_id}")).await;
    assert!(
        form.body
            .contains("value=\"note:view\" aria-label=\"Notes: view\" checked")
    );
    assert!(
        form.body
            .contains("value=\"note:add\" aria-label=\"Notes: add\">")
    );
    assert!(
        form.body
            .contains(&format!("name=\"group\" value=\"{group}\" checked"))
    );
    assert!(
        form.body.contains("value=\"files:view\""),
        "uploads are enabled"
    );

    // Narrowing the group applies to the token sam already holds.
    let reply = root
        .post(
            &app,
            &format!("/admin/auth/groups/{group}"),
            "name=Editors&perm=note:delete",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        status(
            &app,
            &sam,
            "POST",
            "/api/note",
            Some(json!({ "text": "y" }))
        )
        .await,
        403
    );
    assert_eq!(status(&app, &sam, "DELETE", "/api/note/2", None).await, 204);

    // Deleting the group leaves the account with its own permissions only.
    let reply = root
        .post(&app, &format!("/admin/auth/groups/{group}/delete"), "")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(status(&app, &sam, "DELETE", "/api/note/1", None).await, 403);
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 200);

    // Group names are unique and required.
    for fields in ["name=Dup", "name=Dup", "name=+"] {
        let reply = root.post(&app, "/admin/auth/groups/new", fields).await;
        if reply.status != StatusCode::SEE_OTHER {
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(reply.body.contains("class=\"error\""));
        }
    }
    let groups = root.get(&app, "/admin/auth/groups").await;
    assert_eq!(groups.body.matches("<strong>Dup</strong>").count(), 1);
}

#[tokio::test]
async fn changes_to_an_account_take_effect_immediately() {
    let (app, _db, root) = setup().await;
    root.add_user(&app, "sam", "perm=note:view").await;
    let sam = token(&app, "sam", PASSWORD).await.unwrap();
    let browser = Browser::login(&app, "sam", PASSWORD).await;
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 200);

    let users = root.get(&app, "/admin/auth/users").await;
    let id = id_of(&users.body, "users", "sam");
    let path = format!("/admin/auth/users/{id}");

    // Deactivated: the token and the cookie stop working, and logging in is refused.
    let reply = root
        .post(&app, &path, "username=sam&is_staff=true&perm=note:view")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 401);
    assert_eq!(
        browser.get(&app, "/admin/note").await.status,
        StatusCode::SEE_OTHER
    );
    assert!(token(&app, "sam", PASSWORD).await.is_none());

    // Reactivated, with a new password: the old password is gone, the old token works again.
    let reply = root
        .post(&app, &path, "username=sam&password=another+long+password&is_active=true&is_staff=true&perm=note:view")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 200);
    assert!(token(&app, "sam", PASSWORD).await.is_none());
    assert!(token(&app, "sam", "another long password").await.is_some());

    // Saving without a password keeps it.
    let reply = root
        .post(&app, &path, "username=sam&is_active=true&is_staff=true")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(token(&app, "sam", "another long password").await.is_some());
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 403);

    // Not staff: no access at all.
    let reply = root.post(&app, &path, "username=sam&is_active=true").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 401);

    // Deleted.
    let reply = root.post(&app, &format!("{path}/delete"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(status(&app, &sam, "GET", "/api/note", None).await, 401);
    assert!(token(&app, "sam", "another long password").await.is_none());
    assert_eq!(root.get(&app, &path).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_superusers_manage_accounts_and_cannot_lock_themselves_out() {
    let (app, _db, root) = setup().await;
    root.add_user(
        &app,
        "sam",
        "perm=note:view&perm=note:add&perm=note:change&perm=note:delete",
    )
    .await;
    let sam = Browser::login(&app, "sam", PASSWORD).await;

    // Full permissions on a model are not permission to manage accounts.
    for path in [
        "/admin/auth/users",
        "/admin/auth/users/new",
        "/admin/auth/users/1",
        "/admin/auth/groups",
        "/admin/auth/groups/new",
    ] {
        assert_eq!(
            sam.get(&app, path).await.status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let grab =
        "username=eve&password=long+enough+password&is_active=true&is_staff=true&is_superuser=true";
    assert_eq!(
        sam.post(&app, "/admin/auth/users/new", grab).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/auth/users/1", "username=root")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/auth/users/1/delete", "")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.post(&app, "/admin/auth/groups/new", "name=Mine")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(token(&app, "eve", "long enough password").await.is_none());

    // A superuser sees the links and the pages.
    assert!(
        root.get(&app, "/admin/")
            .await
            .body
            .contains("admin/auth/users")
    );
    let users = root.get(&app, "/admin/auth/users").await;
    assert_eq!(users.status, StatusCode::OK);
    let root_id = id_of(&users.body, "users", "root");
    let root_path = format!("/admin/auth/users/{root_id}");

    // ...but cannot remove their own access or delete themselves.
    for fields in [
        "username=root&is_active=true&is_staff=true",
        "username=root&is_staff=true&is_superuser=true",
        "username=root&is_active=true&is_superuser=true",
    ] {
        let reply = root.post(&app, &root_path, fields).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{fields}");
        assert!(reply.body.contains("cannot remove your own access"));
    }
    assert_eq!(
        root.post(&app, &format!("{root_path}/delete"), "")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        root.get(&app, "/admin/auth/users").await.status,
        StatusCode::OK
    );

    // Mistakes in the form are shown, with what was typed, and nothing is saved.
    for (fields, message) in [
        (
            "username=sam&password=long+enough+password&is_active=true&is_staff=true",
            "already exists",
        ),
        (
            "username=new+user&password=long+enough+password",
            "letters, digits",
        ),
        ("username=newuser&password=short", "at least 8"),
        ("username=newuser", "needs a password"),
    ] {
        let reply = root
            .post(
                &app,
                "/admin/auth/users/new",
                &format!("{fields}&perm=task:view"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{fields}");
        assert!(reply.body.contains(message), "{fields}: {}", reply.body);
        assert!(
            reply
                .body
                .contains("value=\"task:view\" aria-label=\"Tasks: view\" checked")
        );
    }
    assert!(
        !root
            .get(&app, "/admin/auth/users")
            .await
            .body
            .contains("newuser")
    );

    // Writes need the CSRF token like everywhere else.
    let request = Request::post("/admin/auth/users/new")
        .header(header::COOKIE, &root.cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(grab))
        .unwrap();
    assert_eq!(send(&app, request).await.0.status, StatusCode::FORBIDDEN);

    // A second superuser can demote the first.
    let reply = root.post(&app, "/admin/auth/users/new", grab).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let eve = Browser::login(&app, "eve", "long enough password").await;
    let reply = eve
        .post(
            &app,
            &root_path,
            "username=root&is_active=true&is_staff=true",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        root.get(&app, "/admin/auth/users").await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn file_permissions_work_like_a_model_named_files() {
    let (app, _db, root) = setup().await;
    root.add_user(&app, "reader", "perm=files:view").await;
    root.add_user(&app, "writer", "perm=files:add").await;
    let reader = token(&app, "reader", PASSWORD).await.unwrap();
    let writer = token(&app, "writer", PASSWORD).await.unwrap();
    let begin = || Some(json!({ "filename": "a.txt", "size": 3, "content_type": "text/plain" }));

    assert_eq!(status(&app, &reader, "GET", "/api/files", None).await, 200);
    assert_eq!(
        status(&app, &reader, "POST", "/api/uploads", begin()).await,
        403
    );
    assert_eq!(
        status(&app, &reader, "DELETE", "/api/files/1", None).await,
        403
    );

    assert_eq!(
        status(&app, &writer, "POST", "/api/uploads", begin()).await,
        201
    );
    assert_eq!(status(&app, &writer, "GET", "/api/files", None).await, 403);
    assert_eq!(
        status(&app, &writer, "DELETE", "/api/files/1", None).await,
        403
    );
}

#[tokio::test]
async fn accounts_created_before_superusers_existed_can_be_promoted() {
    let db = common::database().await;
    // The table as the first release created it, with one account in it.
    let id = match db.get_database_backend() {
        DbBackend::Postgres => "SERIAL PRIMARY KEY",
        DbBackend::MySql => "INTEGER PRIMARY KEY AUTO_INCREMENT",
        _ => "INTEGER PRIMARY KEY AUTOINCREMENT",
    };
    // One statement at a time: MySQL takes no more in one call.
    let sql = format!(
        "CREATE TABLE auth_user (
            id {id},
            username VARCHAR(255) NOT NULL UNIQUE,
            password_hash VARCHAR(255) NOT NULL,
            totp_secret VARCHAR(255),
            is_active BOOLEAN NOT NULL,
            is_staff BOOLEAN NOT NULL
        );
        INSERT INTO auth_user (username, password_hash, is_active, is_staff)
        VALUES ('old', '{}', TRUE, TRUE)",
        hash_password(PASSWORD).unwrap()
    );
    for statement in sql.split(";\n") {
        db.execute_unprepared(statement).await.unwrap();
    }

    let app = new_app(&db);
    app.migrate().await.unwrap();
    let app = app.router().unwrap();

    // The account survives the upgrade and can log in, but has no permissions yet.
    let old = token(&app, "old", PASSWORD)
        .await
        .expect("still able to log in");
    assert_eq!(status(&app, &old, "GET", "/api/note", None).await, 403);

    promote(&db, "old").await.unwrap();
    assert_eq!(status(&app, &old, "GET", "/api/note", None).await, 200);
    let browser = Browser::login(&app, "old", PASSWORD).await;
    assert_eq!(
        browser.get(&app, "/admin/auth/users").await.status,
        StatusCode::OK
    );
    assert!(promote(&db, "nobody").await.is_err());
}

#[tokio::test]
async fn a_custom_account_store_gets_full_access_and_no_account_pages() {
    let db = common::database().await;
    let store = EnvUserStore::new(AdminUser {
        username: "operator".into(),
        password_hash: hash_password(PASSWORD).unwrap(),
        totp_secret: None,
    })
    .unwrap();
    let app = new_app(&db).users(store);
    app.migrate().await.unwrap();
    let app = app.router().unwrap();

    let operator = token(&app, "operator", PASSWORD).await.unwrap();
    assert_eq!(
        status(
            &app,
            &operator,
            "POST",
            "/api/note",
            Some(json!({ "text": "x" }))
        )
        .await,
        201
    );
    assert_eq!(
        status(&app, &operator, "DELETE", "/api/note/1", None).await,
        204
    );

    let browser = Browser::login(&app, "operator", PASSWORD).await;
    assert!(
        !browser
            .get(&app, "/admin/")
            .await
            .body
            .contains("/auth/users")
    );
    assert_eq!(
        browser.get(&app, "/admin/auth/users").await.status,
        StatusCode::NOT_FOUND
    );
}
