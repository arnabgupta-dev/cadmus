//! Many-to-many fields, related records, UUID / decimal / choice fields, save hooks and
//! per-record ownership.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cadmus::{App, AuthConfig, ModelOptions, create_superuser};
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

mod tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "tag")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod post {
    use sea_orm::ActiveValue::Set;
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub slug: String,
        pub status: String,
        #[sea_orm(column_type = "Decimal(Some((10, 2)))")]
        pub price: Decimal,
        pub author_id: Option<i32>,
        pub owner: String,
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

    #[async_trait::async_trait]
    impl ActiveModelBehavior for ActiveModel {
        async fn before_save<C: ConnectionTrait>(
            mut self,
            _db: &C,
            _insert: bool,
        ) -> Result<Self, DbErr> {
            if let sea_orm::ActiveValue::Set(price) | sea_orm::ActiveValue::Unchanged(price) =
                &self.price
                && price.is_sign_negative()
            {
                return Err(cadmus::invalid("price", "must not be negative"));
            }
            // The slug follows the title.
            let title = match &self.title {
                sea_orm::ActiveValue::Set(t) | sea_orm::ActiveValue::Unchanged(t) => t.clone(),
                sea_orm::ActiveValue::NotSet => String::new(),
            };
            self.slug = Set(title.to_lowercase().replace(' ', "-"));
            Ok(self)
        }

        async fn before_delete<C: ConnectionTrait>(self, _db: &C) -> Result<Self, DbErr> {
            if matches!(&self.status, sea_orm::ActiveValue::Unchanged(s) if s == "published") {
                return Err(cadmus::invalid(
                    "status",
                    "published posts cannot be deleted",
                ));
            }
            Ok(self)
        }
    }
}

mod post_tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "post_tag")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub post_id: i32,
        pub tag_id: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::post::Entity",
            from = "Column::PostId",
            to = "super::post::Column::Id",
            on_delete = "Cascade"
        )]
        Post,
        #[sea_orm(
            belongs_to = "super::tag::Entity",
            from = "Column::TagId",
            to = "super::tag::Column::Id",
            on_delete = "Cascade"
        )]
        Tag,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod ticket {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "ticket")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub subject: String,
        pub score: Option<f64>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<author::Entity>(ModelOptions::new().label("Authors"))
        .model::<post::Entity>(
            ModelOptions::new()
                .label("Posts")
                .list_display(["title", "tags", "status"])
                .list_filter(["tags"])
                .readonly_fields(["slug"])
                .choices("status", ["draft", "published"])
                .owned_by("owner")
                .many_to_many::<post_tag::Entity>(
                    "tags",
                    post_tag::Column::PostId,
                    post_tag::Column::TagId,
                ),
        )
        .model::<tag::Entity>(ModelOptions::new().label("Tags"))
        .model::<ticket::Entity>(ModelOptions::new().label("Tickets"))
}

struct Site {
    app: Router,
    root: String,
    browser: Browser,
}

/// Tags `rust` (1), `web` (2) and `db` (3), and author Ada (1).
async fn setup() -> Site {
    let db = common::database().await;
    let app = new_app(&db);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let root = token(&app, "root").await;
    for name in ["rust", "web", "db"] {
        let (status, reply) = api(
            &app,
            &root,
            "POST",
            "/api/tag",
            Some(json!({ "name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }
    let (status, reply) = api(
        &app,
        &root,
        "POST",
        "/api/author",
        Some(json!({ "name": "Ada" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reply}");
    let browser = Browser::login(&app, "root").await;
    Site { app, root, browser }
}

fn post_body(title: &str) -> Value {
    json!({ "title": title, "status": "draft", "price": "19.99", "author_id": 1 })
}

#[tokio::test]
async fn many_to_many_through_the_api() {
    let Site { app, root, .. } = setup().await;
    let mut body = post_body("First post");
    body["tags"] = json!([1, 2]);
    let (status, post) = api(&app, &root, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{post}");
    assert_eq!(post["tags"], json!([1, 2]));
    let id = post["id"].as_i64().unwrap();

    // Replaced by PATCH, kept when left out.
    let path = format!("/api/post/{id}");
    let (status, post) = api(&app, &root, "PATCH", &path, Some(json!({ "tags": [3] }))).await;
    assert_eq!(status, StatusCode::OK, "{post}");
    assert_eq!(post["tags"], json!([3]));
    let (_, post) = api(
        &app,
        &root,
        "PATCH",
        &path,
        Some(json!({ "title": "Renamed" })),
    )
    .await;
    assert_eq!(post["tags"], json!([3]));

    // Filtering by a linked row.
    let (_, list) = api(&app, &root, "GET", "/api/post?tags=3", None).await;
    assert_eq!(list["meta"]["total"], 1);
    let (_, list) = api(&app, &root, "GET", "/api/post?tags=1", None).await;
    assert_eq!(list["meta"]["total"], 0);

    // Ids that do not exist are refused, and nothing changes.
    let (status, error) = api(
        &app,
        &root,
        "PATCH",
        &path,
        Some(json!({ "tags": [3, 99] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["field"], "tags");
    let (_, post) = api(&app, &root, "GET", &path, None).await;
    assert_eq!(post["tags"], json!([3]));
    let (status, _) = api(&app, &root, "PATCH", &path, Some(json!({ "tags": "3" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Deleting a tag removes its links; deleting the post removes the rest.
    let (status, _) = api(&app, &root, "DELETE", "/api/tag/3", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, post) = api(&app, &root, "GET", &path, None).await;
    assert_eq!(post["tags"], json!([]));
    let (_, _) = api(&app, &root, "PATCH", &path, Some(json!({ "tags": [1] }))).await;
    let (status, _) = api(&app, &root, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The API documentation describes the field.
    let (_, docs) = api(&app, &root, "GET", "/api/openapi.json", None).await;
    assert_eq!(
        docs["components"]["schemas"]["Post"]["properties"]["tags"]["type"],
        "array"
    );
}

#[tokio::test]
async fn many_to_many_in_the_admin() {
    let Site { app, browser, .. } = setup().await;
    let form = browser.get(&app, "/admin/post/new").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(
        form.body.contains("name=\"tags__link__1\""),
        "{}",
        form.body
    );
    assert!(form.body.contains("<option value=\"published\""));

    let fields = "title=From+the+admin&status=draft&price=5&author_id=1&tags__links=1&tags__link__1=on&tags__link__3=on";
    let reply = browser.post(&app, "/admin/post/new", fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

    let list = browser.get(&app, "/admin/post").await;
    assert!(list.body.contains("rust, db"), "{}", list.body);
    assert!(
        list.body.contains("<option value=\"2\">web</option>"),
        "tags filter"
    );
    let edit = browser.get(&app, "/admin/post/1").await;
    assert!(
        edit.body
            .contains("name=\"tags__link__3\" value=\"on\" checked")
    );

    // Unticking every box removes every link.
    let fields = "title=From+the+admin&status=draft&price=5&author_id=1&owner=root&tags__links=1";
    let reply = browser.post(&app, "/admin/post/1", fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let edit = browser.get(&app, "/admin/post/1").await;
    assert!(!edit.body.contains("checked"), "{}", edit.body);
}

#[tokio::test]
async fn a_record_lists_what_points_to_it() {
    let Site { app, root, browser } = setup().await;
    for title in ["One", "Two"] {
        let (status, reply) = api(&app, &root, "POST", "/api/post", Some(post_body(title))).await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }
    let page = browser.get(&app, "/admin/author/1").await;
    assert!(
        page.body
            .contains("Posts <span class=\"num muted\">2</span>"),
        "{}",
        page.body
    );
    assert!(page.body.contains(">One</a>") && page.body.contains(">Two</a>"));
    assert!(page.body.contains("admin/post?author_id=1"));
    assert!(page.body.contains("admin/post/new?author_id=1"));

    // The "Add" link starts the form pointing to the author.
    let form = browser.get(&app, "/admin/post/new?author_id=1").await;
    assert!(
        form.body
            .contains("<option value=\"1\" selected>Ada</option>"),
        "{}",
        form.body
    );
    let list = browser.get(&app, "/admin/post?author_id=1").await;
    assert!(list.body.contains(">One<") && list.body.contains(">Two<"));
}

#[tokio::test]
async fn uuid_decimal_and_choice_fields() {
    let Site { app, root, browser } = setup().await;
    // A UUID key left out is made up; one given is kept.
    let (status, ticket) = api(
        &app,
        &root,
        "POST",
        "/api/ticket",
        Some(json!({ "subject": "Help" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    let id = ticket["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 36, "{id}");
    let given = "6f1c9a4e-2d3b-4c5a-9e8f-0a1b2c3d4e5f";
    let body = json!({ "id": given, "subject": "Mine" });
    let (status, ticket) = api(&app, &root, "POST", "/api/ticket", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    assert_eq!(ticket["id"], given);
    let (status, ticket) = api(
        &app,
        &root,
        "PATCH",
        &format!("/api/ticket/{id}"),
        Some(json!({ "subject": "Fixed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    assert_eq!(ticket["subject"], "Fixed");
    let (status, _) = api(
        &app,
        &root,
        "POST",
        "/api/ticket",
        Some(json!({ "id": "nope", "subject": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let reply = browser
        .post(&app, "/admin/ticket/new", "id=&subject=From+the+admin")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

    // Decimals are exact.
    let mut body = post_body("Priced");
    body["price"] = json!("1234.56");
    let (status, post) = api(&app, &root, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{post}");
    assert_eq!(post["price"], "1234.56", "{post}");
    // Floats keep every digit too, on every database.
    let body = json!({ "subject": "Scored", "score": 9.95 });
    let (status, ticket) = api(&app, &root, "POST", "/api/ticket", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    assert_eq!(ticket["score"], json!(9.95), "{ticket}");

    // Choices refuse other values.
    let mut body = post_body("Odd");
    body["status"] = json!("archived");
    let (status, error) = api(&app, &root, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["field"], "status");
}

#[tokio::test]
async fn save_and_delete_hooks() {
    let Site { app, root, browser } = setup().await;
    // `before_save` fills the slug in.
    let (status, post) = api(
        &app,
        &root,
        "POST",
        "/api/post",
        Some(post_body("Hello World")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{post}");
    assert_eq!(post["slug"], "hello-world");
    let id = post["id"].as_i64().unwrap();
    let path = format!("/api/post/{id}");
    let (_, post) = api(
        &app,
        &root,
        "PATCH",
        &path,
        Some(json!({ "title": "New Title" })),
    )
    .await;
    assert_eq!(post["slug"], "new-title");

    // ... and refuses a negative price, as a field error, through the API and the admin.
    let (status, error) = api(&app, &root, "PATCH", &path, Some(json!({ "price": "-1" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["field"], "price");
    assert_eq!(error["error"]["message"], "must not be negative");
    let reply = browser
        .post(
            &app,
            &format!("/admin/post/{id}"),
            "title=X&status=draft&price=-3&owner=root",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        reply.body.contains("must not be negative"),
        "{}",
        reply.body
    );

    // `before_delete` keeps published posts.
    let (_, _) = api(
        &app,
        &root,
        "PATCH",
        &path,
        Some(json!({ "status": "published" })),
    )
    .await;
    let (status, error) = api(&app, &root, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let (status, _) = api(&app, &root, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn accounts_see_only_their_own_rows() {
    let Site { app, root, browser } = setup().await;
    browser
        .add_user(&app, "alice", "perm=post:view&perm=post:add&perm=post:change&perm=post:delete&perm=tag:view&perm=author:view")
        .await;
    let alice = token(&app, "alice").await;

    let (_, theirs) = api(&app, &root, "POST", "/api/post", Some(post_body("Root's"))).await;
    assert_eq!(theirs["owner"], "root");
    // Alice cannot give her post to someone else.
    let mut body = post_body("Alice's");
    body["owner"] = json!("root");
    let (status, mine) = api(&app, &alice, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    assert_eq!(mine["owner"], "alice");

    let (_, list) = api(&app, &alice, "GET", "/api/post", None).await;
    assert_eq!(list["meta"]["total"], 1);
    assert_eq!(list["data"][0]["title"], "Alice's");
    let (_, list) = api(&app, &root, "GET", "/api/post", None).await;
    assert_eq!(list["meta"]["total"], 2);

    let other = format!("/api/post/{}", theirs["id"]);
    for (method, body) in [
        ("GET", None),
        ("PATCH", Some(json!({ "title": "Mine now" }))),
        ("DELETE", None),
    ] {
        let (status, _) = api(&app, &alice, method, &other, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
    let mine_path = format!("/api/post/{}", mine["id"]);
    let (status, post) = api(
        &app,
        &alice,
        "PATCH",
        &mine_path,
        Some(json!({ "owner": "root" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(post["owner"], "alice");

    // The admin too: the list, the record, and its form's owner field.
    let alice_browser = Browser::login(&app, "alice").await;
    let list = alice_browser.get(&app, "/admin/post").await;
    assert!(
        list.body.contains("Alice&#x27;s") || list.body.contains("Alice's"),
        "{}",
        list.body
    );
    assert!(!list.body.contains("Root&#x27;s") && !list.body.contains("Root's"));
    let page = alice_browser
        .get(&app, &format!("/admin/post/{}", theirs["id"]))
        .await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    let form = alice_browser.get(&app, "/admin/post/new").await;
    assert!(form.body.contains("value=\"alice\""), "{}", form.body);
    // A superuser may hand a row over.
    let (status, post) = api(
        &app,
        &root,
        "PATCH",
        &mine_path,
        Some(json!({ "owner": "root" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(post["owner"], "root");
}

// ---- helpers -----------------------------------------------------------------------------

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
        let (reply, cookie) = send(app, request).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let mut browser = Self {
            cookie: cookie.unwrap(),
            csrf: String::new(),
        };
        let home = browser.get(app, "/admin/").await;
        let marker = "name=\"_csrf\" value=\"";
        let start = home.body.find(marker).unwrap() + marker.len();
        browser.csrf = home.body[start..].split('"').next().unwrap().to_owned();
        browser
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        let request = Request::get(path)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        send(app, request).await.0
    }

    async fn post(&self, app: &Router, path: &str, fields: &str) -> Reply {
        let request = Request::post(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("_csrf={}&{fields}", self.csrf)))
            .unwrap();
        send(app, request).await.0
    }

    async fn add_user(&self, app: &Router, username: &str, extra: &str) {
        let fields = format!(
            "username={username}&password={}&is_active=true&is_staff=true&{extra}",
            PASSWORD.replace(' ', "+")
        );
        let reply = self.post(app, "/admin/auth/users/new", &fields).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
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
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

mod board {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "board")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub labels: Vec<String>,
        pub scores: Vec<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Arrays exist only on PostgreSQL.
#[tokio::test]
async fn array_fields_on_postgres() {
    if !std::env::var("TEST_DATABASE_URL").is_ok_and(|url| url.starts_with("postgres")) {
        return;
    }
    let db = common::database().await;
    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<board::Entity>(ModelOptions::new().label("Boards"));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let root = token(&app, "root").await;
    let body = json!({ "labels": ["red", "green"], "scores": [3, 1] });
    let (status, board) = api(&app, &root, "POST", "/api/board", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{board}");
    assert_eq!(board["labels"], json!(["red", "green"]));
    assert_eq!(board["scores"], json!([3, 1]));
    let (status, error) = api(
        &app,
        &root,
        "PATCH",
        "/api/board/1",
        Some(json!({ "scores": ["x"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");

    // The admin edits them one item per line.
    let browser = Browser::login(&app, "root").await;
    let form = browser.get(&app, "/admin/board/1").await;
    assert!(form.body.contains("red\ngreen"), "{}", form.body);
    let reply = browser
        .post(
            &app,
            "/admin/board/1",
            "labels=blue%0D%0A%0D%0Ayellow&scores=7",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let (_, board) = api(&app, &root, "GET", "/api/board/1", None).await;
    assert_eq!(board["labels"], json!(["blue", "yellow"]));
    assert_eq!(board["scores"], json!([7]));
}
