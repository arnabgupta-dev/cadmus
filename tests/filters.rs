//! List filters: dropdowns in the admin (`list_filter`) and query parameters in the API.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use heraapi::{App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
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

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub status: String,
        pub published: bool,
        pub author_id: i32,
        pub views: Option<i32>,
        pub created_at: DateTime,
        pub notes: Json,
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
    token: String,
    session: String,
}

/// Authors Ada (1) and Bob (2), and four posts:
///
/// | id | title  | status | published | author | views | created          |
/// |----|--------|--------|-----------|--------|-------|------------------|
/// | 1  | Alpha  | draft  | no        | Ada    | –     | today            |
/// | 2  | Beta   | live   | yes       | Ada    | 10    | 3 days ago       |
/// | 3  | Gamma  | live   | yes       | Bob    | 250   | 40 days ago      |
/// | 4  | Delta  | review | no        | Bob    | 5     | 400 days ago     |
async fn setup() -> Site {
    let db = common::database().await;
    let app = App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().cookie_secure(false),
    )
    .model::<author::Entity>(ModelOptions::new().label("Authors"))
    .model::<post::Entity>(ModelOptions::new().label("Posts").list_filter([
        "published",
        "author_id",
        "status",
        "created_at",
        "views",
        "notes",
    ]));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    seed(&db).await;
    let app = app.router().unwrap();

    let reply = send(
        &app,
        Request::post("/api/auth/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"username": "root", "password": PASSWORD}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    let token = serde_json::from_str::<Value>(&reply.1).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let login = app
        .clone()
        .oneshot(
            Request::post("/admin/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username=root&password={}",
                    PASSWORD.replace(' ', "+")
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let session = login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    Site {
        app,
        token,
        session,
    }
}

async fn seed(db: &DatabaseConnection) {
    for name in ["Ada", "Bob"] {
        author::ActiveModel {
            name: Set(name.into()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }
    let now = chrono::Utc::now().naive_utc();
    let days = |n: i64| now - chrono::Duration::days(n);
    for (title, status, published, author, views, created) in [
        ("Alpha", "draft", false, 1, None, now),
        ("Beta", "live", true, 1, Some(10), days(3)),
        ("Gamma", "live", true, 2, Some(250), days(40)),
        ("Delta", "review", false, 2, Some(5), days(400)),
    ] {
        post::ActiveModel {
            title: Set(title.into()),
            status: Set(status.into()),
            published: Set(published),
            author_id: Set(author),
            views: Set(views),
            created_at: Set(created),
            notes: Set(json!({})),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The titles the API lists for this query string, in order, or the error status and body.
async fn api(site: &Site, query: &str) -> Result<Vec<String>, (StatusCode, String)> {
    let (status, body) = send(
        &site.app,
        Request::get(format!("/api/post?sort=id&{query}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", site.token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    if status != StatusCode::OK {
        return Err((status, body));
    }
    let json: Value = serde_json::from_str(&body).unwrap();
    Ok(json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["title"].as_str().unwrap().to_owned())
        .collect())
}

async fn page(site: &Site, query: &str) -> String {
    let (status, body) = send(
        &site.app,
        Request::get(format!("/admin/post?{query}"))
            .header(header::COOKIE, &site.session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn the_api_filters_by_any_visible_column() {
    let site = setup().await;
    let titles = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_owned()).collect() };
    assert_eq!(
        api(&site, "published=true").await.unwrap(),
        titles(&["Beta", "Gamma"])
    );
    assert_eq!(
        api(&site, "published=false&author_id=2").await.unwrap(),
        titles(&["Delta"])
    );
    assert_eq!(
        api(&site, "status=live").await.unwrap(),
        titles(&["Beta", "Gamma"])
    );
    assert_eq!(
        api(&site, "views__gte=10").await.unwrap(),
        titles(&["Beta", "Gamma"])
    );
    assert_eq!(
        api(&site, "views__lt=10").await.unwrap(),
        titles(&["Delta"])
    );
    assert_eq!(
        api(&site, "views__isnull=true").await.unwrap(),
        titles(&["Alpha"])
    );
    assert_eq!(api(&site, "views__isnull=false").await.unwrap().len(), 3);
    assert_eq!(
        api(&site, "created_at__range=7d").await.unwrap(),
        titles(&["Alpha", "Beta"])
    );
    let since = (chrono::Utc::now() - chrono::Duration::days(100)).format("%Y-%m-%d");
    assert_eq!(
        api(&site, &format!("created_at__gte={since}"))
            .await
            .unwrap(),
        titles(&["Alpha", "Beta", "Gamma"])
    );
    // Filters combine with search, and the total counts only matches.
    assert_eq!(
        api(&site, "published=true&q=gam").await.unwrap(),
        titles(&["Gamma"])
    );

    for (query, problem) in [
        ("nonsense=1", "not a column this list can be filtered by"),
        ("notes={}", "not a column this list can be filtered by"),
        ("views=many", "not a valid value"),
        ("published=perhaps", "not a valid value"),
        ("views__isnull=maybe", "true or false"),
        ("title__range=7d", "on a date column"),
    ] {
        let (status, body) = api(&site, query).await.unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert!(body.contains(problem), "{query}: {body}");
    }
}

#[tokio::test]
async fn the_admin_offers_dropdowns_for_list_filter_columns() {
    let site = setup().await;
    let body = page(&site, "").await;
    assert!(body.contains("aria-label=\"Filters\""), "{body}");
    // Booleans, foreign keys by name, values present, and periods for dates.
    assert!(body.contains("<option value=\"true\">Yes</option>"));
    assert!(body.contains("<option value=\"2\">Bob</option>"));
    assert!(body.contains("<option value=\"review\">review</option>"));
    assert!(body.contains("name=\"created_at__range\""));
    assert!(body.contains("<option value=\"7d\">Past 7 days</option>"));
    // Nullable columns can be filtered for "empty"; JSON columns cannot be filtered.
    assert!(body.contains("<option value=\"__null\">(empty)</option>"));
    assert!(!body.contains("name=\"notes\""));

    let filtered = page(&site, "published=true&author_id=2").await;
    assert!(
        filtered.contains("<span class=\"num\">1</span> row"),
        "{filtered}"
    );
    assert!(filtered.contains("(filtered)"));
    assert!(filtered.contains("<option value=\"2\" selected>Bob</option>"));
    assert!(filtered.contains("Clear filters"));
    // Sorting and paging links keep the filters.
    assert!(filtered.contains("author_id=2&amp;published=true&amp;sort=title"));
    let empty = page(&site, "published=true&views__isnull=true").await;
    assert!(empty.contains("No posts match these filters"));
    // A stale or hand-edited link shows nothing rather than failing.
    let odd = page(&site, "views=lots&gone=1").await;
    assert!(odd.contains("<span class=\"num\">0</span> rows"));
    let nulls = page(&site, "views=__null").await;
    assert!(nulls.contains("<span class=\"num\">1</span> row"));
}
