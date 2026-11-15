//! Foreign keys in the admin: related rows by name in lists, dropdowns on forms.

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
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        // The other side of the relation is not a column here and must not become a field.
        #[sea_orm(has_many = "super::post::Entity")]
        Post,
    }

    impl Related<super::post::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Post.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod post {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub author_id: i32,
        pub editor_id: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::author::Entity",
            from = "Column::AuthorId",
            to = "super::author::Column::Id"
        )]
        Author,
        #[sea_orm(
            belongs_to = "super::author::Entity",
            from = "Column::EditorId",
            to = "super::author::Column::Id"
        )]
        Editor,
    }

    impl Related<super::author::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Author.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection, authors: ModelOptions) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<author::Entity>(authors.label("Authors"))
        .model::<post::Entity>(ModelOptions::new().label("Posts"))
}

struct Site {
    app: Router,
    db: DatabaseConnection,
    token: String,
    root: Browser,
}

/// Whether a page shows author 2's name or address. (A bare `contains("ada")` would also
/// match the random hex of a CSRF token now and then.)
fn shows_ada(body: &str) -> bool {
    body.contains(">ada<") || body.contains("ada@example.com") || body.contains("\"ada\"")
}

/// Authors Zed (1), ada (2) and Bob (3), and one post by ada that nobody edited.
async fn setup() -> Site {
    let db = common::database().await;
    let app = new_app(&db, ModelOptions::new());
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let token = token(&app, "root").await;
    for name in ["Zed", "ada", "Bob"] {
        let body = json!({ "name": name, "email": format!("{name}@example.com") });
        let (status, reply) = api(&app, &token, "POST", "/api/author", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }
    let body = json!({ "title": "Hello", "author_id": 2 });
    let (status, reply) = api(&app, &token, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{reply}");
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
    let (reply, _) = send(app, request).await;
    (
        reply.status,
        serde_json::from_str(&reply.body).unwrap_or(Value::Null),
    )
}

/// The `<select>` for field `name` in a form page, from its opening tag to its closing one.
fn select<'a>(page: &'a str, name: &str) -> &'a str {
    let start = page
        .find(&format!("<select id=\"f-{name}\""))
        .unwrap_or_else(|| panic!("no dropdown for {name} in {page}"));
    let end = page[start..].find("</select>").unwrap();
    &page[start..start + end]
}

/// The option labels of a dropdown, in order, with `*` after the selected one.
fn options(select: &str) -> Vec<String> {
    select
        .split("<option ")
        .skip(1)
        .map(|option| {
            let label = option.split('>').nth(1).unwrap().split('<').next().unwrap();
            let chosen = option.split('>').next().unwrap().contains(" selected");
            format!("{label}{}", if chosen { "*" } else { "" })
        })
        .collect()
}

#[tokio::test]
async fn lists_name_the_related_row_and_forms_offer_a_dropdown() {
    let site = setup().await;
    let (app, root) = (&site.app, &site.root);

    // The list shows the author's name, linked to the author, instead of the number.
    let list = root.get(app, "/admin/post").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(
        list.body.contains("admin/author/2\">ada</a>"),
        "{}",
        list.body
    );
    // The column is headed by what it points to; an empty foreign key is an empty cell.
    assert!(list.body.contains("<span>Author</span>") && list.body.contains("<span>Editor</span>"));
    assert!(!list.body.contains("Author id"));
    assert!(list.body.contains("data-label=\"Editor\"></td>"));

    // The form offers every author by name, alphabetically whatever the case.
    let form = root.get(app, "/admin/post/new").await;
    let author = select(&form.body, "author_id");
    assert_eq!(options(author), ["Choose…", "ada", "Bob", "Zed"]);
    assert!(author.contains(" required"));
    assert!(author.contains("<option value=\"2\">ada</option>"));
    // An optional one can be left empty.
    let editor = select(&form.body, "editor_id");
    assert_eq!(options(editor), ["— none —", "ada", "Bob", "Zed"]);
    assert!(!editor.contains(" required"));
    assert!(form.body.contains(">Author</label>") && form.body.contains(">Editor</label>"));
    // The other side of the relation did not turn into a field on the author.
    let form = root.get(app, "/admin/author/new").await;
    assert!(!form.body.contains("<select"), "{}", form.body);
    assert_eq!(
        form.body.matches("<label ").count(),
        2,
        "name and email only"
    );

    // Saving what the dropdowns submit works, with and without the optional one.
    let reply = root
        .post(
            app,
            "/admin/post/new",
            "title=Second&author_id=3&editor_id=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let reply = root
        .post(
            app,
            "/admin/post/new",
            "title=Third&author_id=1&editor_id=2",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let (_, rows) = api(app, &site.token, "GET", "/api/post", None).await;
    assert_eq!(
        rows["data"][1],
        json!({ "id": 2, "title": "Second", "author_id": 3, "editor_id": null })
    );
    assert_eq!(
        rows["data"][2],
        json!({ "id": 3, "title": "Third", "author_id": 1, "editor_id": 2 })
    );

    // The edit form has the current rows selected.
    let form = root.get(app, "/admin/post/3").await;
    assert_eq!(
        options(select(&form.body, "author_id")),
        ["Choose…", "ada", "Bob", "Zed*"]
    );
    assert_eq!(
        options(select(&form.body, "editor_id")),
        ["— none —", "ada*", "Bob", "Zed"]
    );
    // Clearing the optional one stores null.
    let reply = root
        .post(app, "/admin/post/3", "title=Third&author_id=1&editor_id=")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let (_, row) = api(app, &site.token, "GET", "/api/post/3", None).await;
    assert_eq!(row["editor_id"], Value::Null);
    // Leaving the required one empty is refused and the form comes back.
    let reply = root
        .post(app, "/admin/post/3", "title=Third&author_id=&editor_id=")
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("<select id=\"f-author_id\""));

    let list = root.get(app, "/admin/post").await;
    for link in [
        "admin/author/2\">ada</a>",
        "admin/author/3\">Bob</a>",
        "admin/author/1\">Zed</a>",
    ] {
        assert!(list.body.contains(link), "{link}");
    }
}

#[tokio::test]
async fn migrate_creates_the_constraint_so_dangling_references_are_refused() {
    let site = setup().await;
    let app = &site.app;

    let body = json!({ "title": "Orphan", "author_id": 999 });
    let (status, reply) = api(app, &site.token, "POST", "/api/post", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["error"]["code"], "conflict");
    let (status, reply) = api(
        app,
        &site.token,
        "PATCH",
        "/api/post/1",
        Some(json!({ "editor_id": 999 })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");

    // An author who still has posts cannot be deleted, from the API or the admin.
    let (status, _) = api(app, &site.token, "DELETE", "/api/author/2", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let reply = site.root.post(app, "/admin/author/2/delete", "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.body.contains("<main id=\"main\""));
    // One without posts can.
    let (status, _) = api(app, &site.token, "DELETE", "/api/author/3", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The same through the form: the dropdown cannot produce it, but a crafted request can.
    let reply = site
        .root
        .post(
            app,
            "/admin/post/new",
            "title=Orphan&author_id=999&editor_id=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("class=\"error\""));
    // What was typed is kept, flagged as pointing at nothing.
    assert!(reply.body.contains("#999 (not found)"), "{}", reply.body);
    let (_, rows) = api(app, &site.token, "GET", "/api/post", None).await;
    assert_eq!(rows["meta"]["total"], 1);
}

#[tokio::test]
async fn the_name_shown_can_be_chosen_and_falls_back_to_the_id() {
    let site = setup().await;
    // The same database, with authors named by e-mail address instead.
    let app = new_app(&site.db, ModelOptions::new().display_field("email"))
        .router()
        .unwrap();
    let list = site.root.get(&app, "/admin/post").await;
    assert!(
        list.body.contains("admin/author/2\">ada@example.com</a>"),
        "{}",
        list.body
    );
    let form = site.root.get(&app, "/admin/post/1").await;
    assert_eq!(
        options(select(&form.body, "author_id")),
        [
            "Choose…",
            "ada@example.com*",
            "Bob@example.com",
            "Zed@example.com"
        ]
    );

    // An author without a name is shown by id rather than as a blank entry.
    let (status, _) = api(
        &site.app,
        &site.token,
        "PATCH",
        "/api/author/2",
        Some(json!({ "name": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = site.root.get(&site.app, "/admin/post").await;
    assert!(
        list.body.contains("admin/author/2\">#2</a>"),
        "{}",
        list.body
    );
    let form = site.root.get(&site.app, "/admin/post/1").await;
    assert_eq!(
        options(select(&form.body, "author_id")),
        ["Choose…", "#2*", "Bob", "Zed"]
    );

    // A hidden column cannot be used to name rows.
    let hidden = ModelOptions::new()
        .display_field("email")
        .hidden_fields(["email"]);
    let app = new_app(&site.db, hidden).router().unwrap();
    let list = site.root.get(&app, "/admin/post").await;
    assert!(!list.body.contains("example.com") && list.body.contains("\">#2</a>"));
}

#[tokio::test]
async fn names_are_only_shown_to_accounts_that_may_view_the_related_model() {
    let site = setup().await;
    let (app, root) = (&site.app, &site.root);
    let fields = format!(
        "username=sam&password={}&is_active=true&is_staff=true&perm=post:view&perm=post:change&perm=post:add",
        PASSWORD.replace(' ', "+")
    );
    let reply = root.post(app, "/admin/auth/users/new", &fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let sam = Browser::login(app, "sam").await;

    // No permission on authors: the plain number, no names, no link.
    let list = sam.get(app, "/admin/post").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(
        list.body.contains("data-label=\"Author\">2</td>"),
        "{}",
        list.body
    );
    assert!(!shows_ada(&list.body) && !list.body.contains("admin/author/"));
    let form = sam.get(app, "/admin/post/1").await;
    assert!(!form.body.contains("<select") && !shows_ada(&form.body));
    assert!(form.body.contains("name=\"author_id\" value=\"2\""));
    // The number can still be changed.
    let reply = sam
        .post(app, "/admin/post/1", "title=Hello&author_id=3&editor_id=")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

    // With permission to view authors, the names appear.
    let users = root.get(app, "/admin/auth/users").await;
    let row = users
        .body
        .split("<tr>")
        .find(|r| r.contains("<strong>sam</strong>"))
        .unwrap();
    let marker = "admin/auth/users/";
    let id = row[row.find(marker).unwrap() + marker.len()..]
        .split('"')
        .next()
        .unwrap();
    let fields = "username=sam&is_active=true&is_staff=true&perm=post:view&perm=author:view";
    let reply = root
        .post(app, &format!("/admin/auth/users/{id}"), fields)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let list = sam.get(app, "/admin/post").await;
    assert!(
        list.body.contains("admin/author/3\">Bob</a>"),
        "{}",
        list.body
    );
}

#[tokio::test]
async fn a_long_list_of_related_rows_is_not_put_in_a_dropdown() {
    let site = setup().await;
    let (app, root) = (&site.app, &site.root);
    // 3 authors exist; 200 is the most a dropdown holds.
    for i in 3..200 {
        let body = json!({ "name": format!("Author {i:03}"), "email": "x@example.com" });
        let (status, _) = api(app, &site.token, "POST", "/api/author", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let form = root.get(app, "/admin/post/1").await;
    assert_eq!(options(select(&form.body, "author_id")).len(), 201);

    let body = json!({ "name": "One too many", "email": "x@example.com" });
    api(app, &site.token, "POST", "/api/author", Some(body)).await;
    let form = root.get(app, "/admin/post/1").await;
    assert!(!form.body.contains("<select"));
    assert!(form.body.contains("name=\"author_id\" value=\"2\""));
    assert!(
        form.body
            .contains("Currently: ada. There are too many Authors to list here"),
        "{}",
        form.body
    );
    // An empty optional key has nothing to name.
    assert!(
        form.body
            .contains("<p class=\"muted\">There are too many Authors to list here")
    );
    // The list still names them.
    assert!(
        root.get(app, "/admin/post")
            .await
            .body
            .contains("admin/author/2\">ada</a>")
    );
}

#[tokio::test]
async fn a_key_to_a_model_that_is_not_registered_stays_a_plain_field() {
    let site = setup().await;
    let app = App::new(site.db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<post::Entity>(ModelOptions::new().label("Posts"))
        .router()
        .unwrap();
    let list = site.root.get(&app, "/admin/post").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("data-label=\"Author\">2</td>") && !shows_ada(&list.body));
    let form = site.root.get(&app, "/admin/post/1").await;
    assert!(!form.body.contains("<select") && form.body.contains("name=\"author_id\" value=\"2\""));
}
