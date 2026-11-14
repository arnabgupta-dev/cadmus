//! File fields on admin forms: picking, replacing and removing a file, and showing it.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use cadmus::{AdminError, App, AuthConfig, ModelOptions, Storage, Uploads, create_superuser};
use http_body_util::BodyExt;
use sea_orm::{DatabaseConnection, EntityTrait, PaginatorTrait};
use tower::ServiceExt;

mod common;

mod product {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "product")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        pub photo: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
const BOUNDARY: &str = "----cadmus-test";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a picture";

fn app(db: &DatabaseConnection, max_file_bytes: u64) -> App {
    App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().cookie_secure(false),
    )
    .uploads(Uploads::new(db.clone(), Storage::memory()).max_file_bytes(max_file_bytes))
    .model::<product::Entity>(ModelOptions::new().label("Products").file_fields(["photo"]))
}

struct Site {
    app: Router,
    db: DatabaseConnection,
    cookie: String,
    csrf: String,
}

async fn setup(max_file_bytes: u64) -> Site {
    let db = common::database().await;
    let app = app(&db, max_file_bytes);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let login = send(
        &app,
        Request::post("/admin/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username=root&password={}",
                PASSWORD.replace(' ', "+")
            )))
            .unwrap(),
    )
    .await;
    let cookie = login
        .headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let home = get(&app, "/admin/", &cookie).await;
    let csrf = home
        .text()
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    Site {
        app,
        db,
        cookie,
        csrf,
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

async fn get(app: &Router, path: &str, cookie: &str) -> Reply {
    send(
        app,
        Request::get(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// A multipart form: text fields, then files as `(field, filename, content type, bytes)`.
fn multipart(fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    for (name, filename, content_type, bytes) in files {
        body.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn submit(site: &Site, path: &str, body: Vec<u8>) -> Reply {
    send(
        &site.app,
        Request::post(path)
            .header(header::COOKIE, &site.cookie)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

async fn product(db: &DatabaseConnection, id: i32) -> product::Model {
    product::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn file_count(db: &DatabaseConnection) -> u64 {
    cadmus::file::Entity::find().count(db).await.unwrap()
}

#[tokio::test]
async fn a_file_is_picked_shown_replaced_and_removed() {
    let site = setup(1 << 20).await;
    let form = get(&site.app, "/admin/product/new", &site.cookie).await;
    assert!(form.text().contains("enctype=\"multipart/form-data\""));
    assert!(form.text().contains("name=\"photo__upload\" type=\"file\""));

    let csrf = site.csrf.as_str();
    let created = submit(
        &site,
        "/admin/product/new",
        multipart(
            &[("_csrf", csrf), ("name", "Lamp"), ("photo", "")],
            &[("photo__upload", "lamp.png", "image/png", PNG)],
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let lamp = product(&site.db, 1).await;
    let photo = lamp.photo.expect("the new file's id");

    // The list names the file and links to it; the form shows it with a preview.
    let list = get(&site.app, "/admin/product", &site.cookie).await;
    assert!(list.text().contains(">lamp.png</a>"), "{}", list.text());
    let edit = get(&site.app, "/admin/product/1", &site.cookie).await;
    assert!(edit.text().contains("lamp.png"));
    assert!(edit.text().contains("photo?inline=1"));
    assert!(edit.text().contains("Remove the file"));

    let download = get(&site.app, "/admin/product/1/file/photo", &site.cookie).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, PNG);
    assert!(
        download
            .header(header::CONTENT_DISPOSITION)
            .starts_with("attachment")
    );
    let preview = get(
        &site.app,
        "/admin/product/1/file/photo?inline=1",
        &site.cookie,
    )
    .await;
    assert!(
        preview
            .header(header::CONTENT_DISPOSITION)
            .starts_with("inline")
    );

    // Saving without picking a file keeps it.
    let kept = submit(
        &site,
        "/admin/product/1",
        multipart(
            &[
                ("_csrf", csrf),
                ("name", "Desk lamp"),
                ("photo", &photo.to_string()),
            ],
            &[],
        ),
    )
    .await;
    assert_eq!(kept.status, StatusCode::SEE_OTHER, "{}", kept.text());
    assert_eq!(product(&site.db, 1).await.photo, Some(photo));

    // Picking another replaces it.
    let replaced = submit(
        &site,
        "/admin/product/1",
        multipart(
            &[
                ("_csrf", csrf),
                ("name", "Desk lamp"),
                ("photo", &photo.to_string()),
            ],
            &[(
                "photo__upload",
                "notes.html",
                "text/html",
                b"<script>alert(1)</script>",
            )],
        ),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::SEE_OTHER);
    let new_photo = product(&site.db, 1).await.photo.unwrap();
    assert_ne!(new_photo, photo);
    // Not an image: never shown inline, whatever is asked.
    let html = get(
        &site.app,
        "/admin/product/1/file/photo?inline=1",
        &site.cookie,
    )
    .await;
    assert!(
        html.header(header::CONTENT_DISPOSITION)
            .starts_with("attachment")
    );
    assert_eq!(
        html.header(header::HeaderName::from_static("x-content-type-options")),
        "nosniff"
    );

    // "Remove the file" empties the column.
    let removed = submit(
        &site,
        "/admin/product/1",
        multipart(
            &[
                ("_csrf", csrf),
                ("name", "Desk lamp"),
                ("photo", &new_photo.to_string()),
                ("photo__clear", "true"),
            ],
            &[],
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert_eq!(product(&site.db, 1).await.photo, None);
    assert_eq!(
        get(&site.app, "/admin/product/1/file/photo", &site.cookie)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn files_of_rejected_forms_are_not_kept() {
    let site = setup(1 << 20).await;
    // The name is required, so the form is shown again and the file removed.
    let rejected = submit(
        &site,
        "/admin/product/new",
        multipart(
            &[("_csrf", &site.csrf), ("photo", "")],
            &[("photo__upload", "a.png", "image/png", PNG)],
        ),
    )
    .await;
    assert_eq!(
        rejected.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        rejected.text()
    );
    assert_eq!(file_count(&site.db).await, 0);

    // A forged form: refused, and nothing stored, even if the file comes first.
    let forged = submit(
        &site,
        "/admin/product/new",
        multipart(
            &[("name", "Lamp")],
            &[("photo__upload", "a.png", "image/png", PNG)],
        ),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(file_count(&site.db).await, 0);
    assert_eq!(product::Entity::find().count(&site.db).await.unwrap(), 0);
}

#[tokio::test]
async fn files_over_the_limit_are_refused() {
    let site = setup(16).await;
    let reply = submit(
        &site,
        "/admin/product/new",
        multipart(
            &[("_csrf", &site.csrf), ("name", "Lamp"), ("photo", "")],
            &[("photo__upload", "big.png", "image/png", &[0_u8; 64])],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(file_count(&site.db).await, 0);
}

#[tokio::test]
async fn file_fields_need_uploads_and_an_integer_column() {
    let db = common::database().await;
    let without_uploads = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<product::Entity>(ModelOptions::new().file_fields(["photo"]));
    assert!(
        matches!(without_uploads.router(), Err(AdminError::Config(m)) if m.contains("uploads"))
    );
    let text_column = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .uploads(Uploads::new(db.clone(), Storage::memory()))
        .model::<product::Entity>(ModelOptions::new().file_fields(["name"]));
    assert!(matches!(text_column.router(), Err(AdminError::Config(m)) if m.contains("integer")));
}
