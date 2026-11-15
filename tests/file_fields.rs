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
    let (cookie, csrf) = login(&app, "root").await;
    Site {
        app,
        db,
        cookie,
        csrf,
    }
}

/// Logs in to the admin; the login cookie and the CSRF token for its forms.
async fn login(app: &Router, username: &str) -> (String, String) {
    let login = send(
        app,
        Request::post("/admin/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username={username}&password={}",
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
    let home = get(app, "/admin/", &cookie).await;
    let csrf = home
        .text()
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    (cookie, csrf)
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
    submit_as(&site.app, &site.cookie, path, body).await
}

async fn submit_as(app: &Router, cookie: &str, path: &str, body: Vec<u8>) -> Reply {
    send(
        app,
        Request::post(path)
            .header(header::COOKIE, cookie)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

/// An API request with a login token and an optional JSON body.
async fn api(app: &Router, token: &str, method: &str, path: &str, body: Option<&str>) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    send(
        app,
        request
            .body(Body::from(body.unwrap_or_default().to_owned()))
            .unwrap(),
    )
    .await
}

async fn token(app: &Router, username: &str) -> String {
    let body = format!(r#"{{"username":"{username}","password":"{PASSWORD}"}}"#);
    let reply = send(
        app,
        Request::post("/api/auth/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let json: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    json["access_token"].as_str().unwrap().to_owned()
}

/// Adds an active staff account through the Users page, with these permissions.
async fn add_staff(site: &Site, username: &str, perms: &[&str]) {
    let mut fields = format!(
        "_csrf={}&username={username}&password={}&is_active=true&is_staff=true",
        site.csrf,
        PASSWORD.replace(' ', "+")
    );
    for perm in perms {
        fields.push_str(&format!("&perm={perm}"));
    }
    let reply = send(
        &site.app,
        Request::post("/admin/auth/users/new")
            .header(header::COOKIE, &site.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(fields))
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
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

#[tokio::test]
async fn a_file_field_names_only_files_its_writer_may_see() {
    let site = setup(1 << 20).await;
    // root's product, with a file only root and accounts that may view files can download.
    let created = submit(
        &site,
        "/admin/product/new",
        multipart(
            &[("_csrf", &site.csrf), ("name", "Payroll"), ("photo", "")],
            &[("photo__upload", "salaries.png", "image/png", PNG)],
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let secret = product(&site.db, 1).await.photo.unwrap();

    // clerk may work with products but not files.
    add_staff(
        &site,
        "clerk",
        &["product:view", "product:add", "product:change"],
    )
    .await;
    let clerk = token(&site.app, "clerk").await;
    let refused = |reply: &Reply| {
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text()
        );
        assert!(
            reply.text().contains(r#""field":"photo""#),
            "{}",
            reply.text()
        );
    };

    // Through the API: someone else's file and a file that does not exist get the same answer.
    let stolen = api(
        &site.app,
        &clerk,
        "POST",
        "/api/product",
        Some(&format!(r#"{{"name":"Trojan","photo":{secret}}}"#)),
    )
    .await;
    refused(&stolen);
    let missing = api(
        &site.app,
        &clerk,
        "POST",
        "/api/product",
        Some(r#"{"name":"Ghost","photo":999}"#),
    )
    .await;
    refused(&missing);
    assert_eq!(
        stolen.text().replace(&secret.to_string(), "N"),
        missing.text().replace("999", "N")
    );
    let own = api(
        &site.app,
        &clerk,
        "POST",
        "/api/product",
        Some(r#"{"name":"Mine"}"#),
    )
    .await;
    assert_eq!(own.status, StatusCode::CREATED, "{}", own.text());
    refused(
        &api(
            &site.app,
            &clerk,
            "PATCH",
            "/api/product/2",
            Some(&format!(r#"{{"photo":{secret}}}"#)),
        )
        .await,
    );
    assert_eq!(product(&site.db, 2).await.photo, None);

    // Through the admin form, by editing the hidden field.
    let (cookie, csrf) = login(&site.app, "clerk").await;
    let forged = submit_as(
        &site.app,
        &cookie,
        "/admin/product/new",
        multipart(
            &[
                ("_csrf", &csrf),
                ("name", "Trojan"),
                ("photo", &secret.to_string()),
            ],
            &[],
        ),
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNPROCESSABLE_ENTITY);
    let forged = submit_as(
        &site.app,
        &cookie,
        "/admin/product/2",
        multipart(
            &[
                ("_csrf", &csrf),
                ("name", "Mine"),
                ("photo", &secret.to_string()),
            ],
            &[],
        ),
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(product::Entity::find().count(&site.db).await.unwrap(), 2);
    assert_eq!(product(&site.db, 2).await.photo, None);
    let download = get(&site.app, "/admin/product/2/file/photo", &cookie).await;
    assert_eq!(download.status, StatusCode::NOT_FOUND);

    // A row's own file may stay: editing root's product keeps its photo.
    let kept = submit_as(
        &site.app,
        &cookie,
        "/admin/product/1",
        multipart(
            &[
                ("_csrf", &csrf),
                ("name", "Payroll 2026"),
                ("photo", &secret.to_string()),
            ],
            &[],
        ),
    )
    .await;
    assert_eq!(kept.status, StatusCode::SEE_OTHER, "{}", kept.text());
    let kept = api(
        &site.app,
        &clerk,
        "PUT",
        "/api/product/1",
        Some(&format!(r#"{{"name":"Payroll","photo":{secret}}}"#)),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.text());
    assert_eq!(product(&site.db, 1).await.photo, Some(secret));

    // A file clerk uploaded may go in any of clerk's rows.
    let uploaded = submit_as(
        &site.app,
        &cookie,
        "/admin/product/2",
        multipart(
            &[("_csrf", &csrf), ("name", "Mine"), ("photo", "")],
            &[("photo__upload", "mine.png", "image/png", PNG)],
        ),
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::SEE_OTHER,
        "{}",
        uploaded.text()
    );
    let mine = product(&site.db, 2).await.photo.unwrap();
    let moved = api(
        &site.app,
        &clerk,
        "POST",
        "/api/product",
        Some(&format!(r#"{{"name":"Copy","photo":{mine}}}"#)),
    )
    .await;
    assert_eq!(moved.status, StatusCode::CREATED, "{}", moved.text());

    // Accounts that may view files may name any of them.
    add_staff(&site, "archivist", &["product:add", "files:view"]).await;
    let archivist = token(&site.app, "archivist").await;
    let linked = api(
        &site.app,
        &archivist,
        "POST",
        "/api/product",
        Some(&format!(r#"{{"name":"Archive","photo":{secret}}}"#)),
    )
    .await;
    assert_eq!(linked.status, StatusCode::CREATED, "{}", linked.text());
}
