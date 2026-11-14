//! Bulk actions on the rows selected in a list: "Delete selected", `set_action` and `action`.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use heraapi::{AdminError, App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
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
    pub enum Relation {
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
        pub published: bool,
        pub author_id: i32,
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

    impl Related<super::author::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Author.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

struct Site {
    app: Router,
    db: DatabaseConnection,
    archived: Arc<Mutex<Vec<String>>>,
}

fn options(archived: &Arc<Mutex<Vec<String>>>) -> ModelOptions {
    let seen = Arc::clone(archived);
    ModelOptions::new()
        .label("Posts")
        .set_action("publish", "Mark as published", "published", "true")
        .action("archive", "Archive", move |ids| {
            let seen = Arc::clone(&seen);
            async move {
                if ids.iter().any(|id| id == "4") {
                    return Err(AdminError::Conflict("post 4 cannot be archived".into()));
                }
                seen.lock().unwrap().extend(ids);
                Ok(())
            }
        })
}

/// Authors Ada (1, with posts) and Bob (2, without), and posts 1–4 by Ada, unpublished.
async fn setup() -> Site {
    let db = common::database().await;
    let archived = Arc::new(Mutex::new(Vec::new()));
    let app = App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().cookie_secure(false),
    )
    .model::<author::Entity>(ModelOptions::new().label("Authors"))
    .model::<post::Entity>(options(&archived));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    for name in ["Ada", "Bob"] {
        author::ActiveModel {
            name: Set(name.into()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }
    for title in ["One", "Two", "Three", "Four"] {
        post::ActiveModel {
            title: Set(title.into()),
            published: Set(false),
            author_id: Set(1),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }
    Site {
        app: app.router().unwrap(),
        db,
        archived,
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

struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn login(app: &Router, username: &str, password: &str) -> Self {
        let reply = send(
            app,
            Request::post("/admin/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={}",
                    password.replace(' ', "+")
                )))
                .unwrap(),
        )
        .await;
        let cookie = reply
            .headers
            .get(header::SET_COOKIE)
            .expect("logged in")
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
        browser.csrf = home
            .body
            .split("name=\"_csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        browser
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        send(
            app,
            Request::get(path)
                .header(header::COOKIE, &self.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn bulk(
        &self,
        app: &Router,
        model: &str,
        action: &str,
        ids: &[&str],
        back: &str,
    ) -> Reply {
        let mut body = format!(
            "_csrf={}&action={action}&back={}",
            self.csrf,
            back.replace('&', "%26").replace('=', "%3D")
        );
        for id in ids {
            body.push_str(&format!("&ids={id}"));
        }
        send(
            app,
            Request::post(format!("/admin/{model}/bulk"))
                .header(header::COOKIE, &self.cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }
}

fn location(reply: &Reply) -> &str {
    reply
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

async fn published(db: &DatabaseConnection) -> Vec<i32> {
    let mut ids: Vec<i32> = post::Entity::find()
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.published)
        .map(|p| p.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn the_list_offers_the_actions_the_account_may_run() {
    let site = setup().await;
    let root = Browser::login(&site.app, "root", PASSWORD).await;
    let page = root.get(&site.app, "/admin/post").await;
    assert!(
        page.body.contains("data-pick-all aria-label"),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains("name=\"ids\" value=\"3\" form=\"bulk-form\"")
    );
    assert!(
        page.body
            .contains("<option value=\"publish\">Mark as published</option>")
    );
    assert!(
        page.body
            .contains("<option value=\"archive\">Archive</option>")
    );
    assert!(
        page.body
            .contains("<option value=\"delete\">Delete selected</option>")
    );
    // Models without actions of their own still get "Delete selected".
    let authors = root.get(&site.app, "/admin/author").await;
    assert!(
        authors
            .body
            .contains("<option value=\"delete\">Delete selected</option>")
    );
    assert!(!authors.body.contains("Mark as published"));
}

#[tokio::test]
async fn set_actions_change_every_selected_row() {
    let site = setup().await;
    let root = Browser::login(&site.app, "root", PASSWORD).await;
    let reply = root
        .bulk(&site.app, "post", "publish", &["1", "3"], "q=o&sort=-title")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    // Back to the same list, with what happened.
    assert_eq!(
        location(&reply),
        "/admin/post?q=o&sort=-title&done=publish&n=2&failed=0"
    );
    assert_eq!(published(&site.db).await, [1, 3]);
    let page = root.get(&site.app, location(&reply)).await;
    assert!(
        page.body.contains("Mark as published: 2 rows."),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn custom_actions_get_the_selected_keys() {
    let site = setup().await;
    let root = Browser::login(&site.app, "root", PASSWORD).await;
    let reply = root
        .bulk(&site.app, "post", "archive", &["2", "1"], "")
        .await;
    assert_eq!(location(&reply), "/admin/post?done=archive&n=2&failed=0");
    assert_eq!(*site.archived.lock().unwrap(), ["1", "2"]);

    let refused = root
        .bulk(&site.app, "post", "archive", &["3", "4"], "")
        .await;
    assert_eq!(location(&refused), "/admin/post?done=archive&n=0&failed=2");
    let page = root.get(&site.app, location(&refused)).await;
    assert!(page.body.contains("Archive: 0 rows. 2 rows failed."));

    let unknown = root.bulk(&site.app, "post", "explode", &["1"], "").await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Nothing selected: back to the list, nothing done.
    let nothing = root
        .bulk(&site.app, "post", "publish", &[], "sort=title")
        .await;
    assert_eq!(location(&nothing), "/admin/post?sort=title");
    assert!(published(&site.db).await.is_empty());
}

#[tokio::test]
async fn delete_selected_keeps_rows_others_point_to() {
    let site = setup().await;
    let root = Browser::login(&site.app, "root", PASSWORD).await;
    let reply = root
        .bulk(&site.app, "author", "delete", &["1", "2"], "")
        .await;
    assert_eq!(location(&reply), "/admin/author?done=delete&n=1&failed=1");
    let page = root.get(&site.app, location(&reply)).await;
    assert!(
        page.body
            .contains("Deleted 1 row. 1 row could not be deleted"),
        "{}",
        page.body
    );
    assert_eq!(author::Entity::find().all(&site.db).await.unwrap().len(), 1);

    let posts = root
        .bulk(&site.app, "post", "delete", &["1", "2", "99"], "")
        .await;
    assert_eq!(location(&posts), "/admin/post?done=delete&n=2&failed=1");
    assert_eq!(post::Entity::find().all(&site.db).await.unwrap().len(), 2);
}

#[tokio::test]
async fn bulk_actions_need_the_permission_and_the_csrf_token() {
    let site = setup().await;
    let root = Browser::login(&site.app, "root", PASSWORD).await;
    // A staff account that may view and change posts, but not delete them.
    let created = send(
        &site.app,
        Request::post("/admin/auth/users/new")
            .header(header::COOKIE, &root.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "_csrf={}&username=sam&password=sam-password-1&is_active=true&is_staff=true&perm=post:view&perm=author:view",
                root.csrf
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let sam = Browser::login(&site.app, "sam", "sam-password-1").await;
    let page = sam.get(&site.app, "/admin/post").await;
    assert!(
        !page.body.contains("name=\"ids\""),
        "view only: no selection"
    );
    assert_eq!(
        sam.bulk(&site.app, "post", "publish", &["1"], "")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sam.bulk(&site.app, "post", "delete", &["1"], "")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(published(&site.db).await.is_empty());

    let forged = send(
        &site.app,
        Request::post("/admin/post/bulk")
            .header(header::COOKIE, &root.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=delete&ids=1"))
            .unwrap(),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(post::Entity::find().all(&site.db).await.unwrap().len(), 4);
}

#[tokio::test]
async fn misconfigured_actions_are_refused_at_start() {
    let db = common::database().await;
    for options in [
        ModelOptions::new().set_action("publish", "Publish", "nonexistent", "true"),
        ModelOptions::new().set_action("publish", "Publish", "id", "1"),
        ModelOptions::new()
            .readonly_fields(["published"])
            .set_action("publish", "Publish", "published", "true"),
        ModelOptions::new().set_action("delete", "Delete", "published", "true"),
        ModelOptions::new()
            .set_action("go", "Go", "published", "true")
            .set_action("go", "Go again", "published", "false"),
    ] {
        let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
            .model::<author::Entity>(ModelOptions::new())
            .model::<post::Entity>(options);
        assert!(matches!(app.router(), Err(AdminError::Config(_))));
    }
}
