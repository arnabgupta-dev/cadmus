---
title: Models
section: Guides
order: 40
summary: Describe your data; every model gets a table, admin pages and API endpoints. Field types and ModelOptions.
---

A model describes one kind of record: its fields and their types. Cadmus uses
[SeaORM](https://www.sea-ql.org/SeaORM/) entities as models.

## Generating a model

```sh
cadmus generate model Post title:string body:text published:bool views:int rating:float?
```

This writes `src/models/post.rs`:

```rust
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "post")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub title: String,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    pub published: bool,
    pub views: i32,
    pub rating: Option<f64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
```

and registers it in `src/models/mod.rs`. Model names are PascalCase (`BlogPost`); the table
is named in snake_case (`blog_post`). An `id` primary key is always added.

## Field types

| Type | Rust type | Stored as |
|---|---|---|
| `string` | `String` | short text (`varchar`) |
| `text` | `String` | long text |
| `int` | `i32` | 32-bit whole number |
| `bigint` | `i64` | 64-bit whole number |
| `float` | `f64` | number with a fraction |
| `bool` | `bool` | true or false |
| `datetime` | `DateTime` | date and time, without a time zone |
| `date` | `Date` | a date |
| `json` | `Json` | a JSON document |
| `file` | `Option<i32>` | an uploaded file (see [File uploads](file-uploads.md)) |
| `references` | `i32` | a link to another model (see [Relations](relations.md)) |

Add `?` to make a field optional (`Option<...>` in Rust, `NULL` in the database):
`summary:text?`. File fields are always optional.

## Changing a model

Edit the file by hand: add, remove, rename or change fields, then make and apply a
migration:

```sh
cargo run -- makemigrations
cargo run -- migrate
```

Useful SeaORM attributes on a field:

| Attribute | Effect |
|---|---|
| `#[sea_orm(unique)]` | no two rows may have the same value |
| `#[sea_orm(indexed)]` | an index, for faster lookups and sorting |
| `#[sea_orm(default_value = "draft")]` | the value new rows get; also fills existing rows when the field is added |
| `#[sea_orm(column_type = "Text")]` | long text instead of `varchar` |
| `#[sea_orm(renamed_from = "body")]` | tells `makemigrations` the field was renamed, so the data is kept |

## Deleting a model

`cadmus delete model` undoes `cadmus generate model`: it deletes the model's file and removes
its two lines from `src/models/mod.rs`, including any admin options you added there. Then make
and apply a migration, which drops the table:

```sh
cadmus delete model Post
cargo run -- makemigrations
cargo run -- migrate
```

It asks before deleting anything (`--yes` skips the question). **The migration deletes the
table and every row in it**, so back up anything you want to keep first. Until you run
`migrate`, the data is still there, and if the project is in git,
`git checkout -- src/models` brings the model back.

It refuses, changing nothing, while other code still uses the model, and lists where: for
example a model with a foreign key to it (`author:references`), or your own routes. Delete or
change those first. Files uploaded to the model's file fields stay in storage.

## Admin options

Each model is registered with `ModelOptions`, which decide how it appears in the admin (and
partly in the API):

```rust
app.model::<post::Entity>(
    ModelOptions::new()
        .label("Posts")                                   // name in the menu
        .list_display(["id", "title", "published"])       // columns on the list page
        .search_fields(["title", "body"])                 // what the search box looks in
        .list_filter(["published", "created_at"])         // filter dropdowns above the list
        .readonly_fields(["views"])                       // shown, but not editable
        .hidden_fields(["internal_note"])                 // never shown or sent anywhere
        .display_field("title")                           // how other models name a post
        .per_page(50)                                     // rows per list page
        .set_action("publish", "Mark as published", "published", "true"),
)
```

| Option | What it does |
|---|---|
| `label("Posts")` | the name shown in the menu and headings |
| `list_display([...])` | the columns on the list page (default: all visible fields) |
| `search_fields([...])` | the text fields the search box looks in; also `?q=` in the API |
| `list_filter([...])` | a filter dropdown per field above the list (see [The admin site](admin.md)) |
| `readonly_fields([...])` | shown but not editable, in the admin and the API |
| `hidden_fields([...])` | never shown or accepted, in the admin, the API or its documentation |
| `display_field("email")` | the field that names a record where other models link to it (default: the first text field) |
| `per_page(n)` | rows per list page for this model |
| `file_fields([...])` | integer fields that hold an uploaded file (set by `field:file`) |
| `set_action(name, label, field, value)` | a bulk action setting a field on the selected rows |
| `action(name, label, \|ids\| async move { ... })` | a bulk action running your own code with the selected ids |

## Models without the generator

Any SeaORM entity can be registered with `.model::<your_entity::Entity>(...)`, including
ones you wrote by hand. Relations declared with `belongs_to` are picked up automatically.
Models must have a single-column primary key to be edited.

A resource that is not a database table at all can be served by implementing the `Resource`
trait and registering it with `App::resource`; see the
[API reference](https://docs.rs/cadmus).
