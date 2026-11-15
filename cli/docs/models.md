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
| `decimal` | `Decimal` | an exact number, such as a price (12 digits, 2 after the point) |
| `uuid` | `Uuid` | a universally unique id |
| `html` | `String` | formatted text, such as a blog post's body, edited with a rich-text editor (see [Rich text](#rich-text)) |
| `file` | `Option<i32>` | an uploaded file (see [File uploads](file-uploads.md)) |
| `references` | `i32` | a link to another model (see [Relations](relations.md)) |
| `many` | (a link table) | links to any number of rows of another model: `tags:many` (see [Relations](relations.md#many-to-many)) |
| `owner` | `String` | the account a row belongs to (see [Rows owned by accounts](accounts.md#rows-owned-by-accounts)) |

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

## Checks and hooks

To check values or fill some in on every save, whether from the admin or the API, use
SeaORM's hooks in the model file. Return `cadmus::invalid(field, message)` to refuse a save:
the admin shows the message on the form and the API answers `422` with it.

```rust
use sea_orm::ActiveValue::{Set, Unchanged};

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    async fn before_save<C: ConnectionTrait>(mut self, _db: &C, _insert: bool) -> Result<Self, DbErr> {
        if let Set(price) | Unchanged(price) = &self.price && price.is_sign_negative() {
            return Err(cadmus::invalid("price", "must not be negative"));
        }
        // The slug follows the title.
        if let Set(title) | Unchanged(title) = &self.title {
            self.slug = Set(title.to_lowercase().replace(' ', "-"));
        }
        Ok(self)
    }

    async fn before_delete<C: ConnectionTrait>(self, _db: &C) -> Result<Self, DbErr> {
        if matches!(&self.status, Unchanged(s) if s == "published") {
            return Err(cadmus::invalid("status", "published posts cannot be deleted"));
        }
        Ok(self)
    }
}
```

The hook sees the whole record: when a record is edited, the values that did not change are
`Unchanged`, and the new ones `Set`. `after_save` and `after_delete` run after the change,
inside the same transaction, so an error there undoes it. Add `async-trait = "0.1"` to
`Cargo.toml` to write them. Bulk actions that set a field run the hooks too; your own
`action` code runs whatever it runs.

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
| `rich_text([...])` | text fields that hold formatted text, edited with a rich-text editor (set by `field:html`; see [Rich text](#rich-text)) |
| `choices(field, [...])` | a dropdown of these values, and no others, for a text or number field; database enums get theirs automatically |
| `many_to_many::<link::Entity>(name, from, to)` | a many-to-many field kept in a link table (set by `field:many`; see [Relations](relations.md#many-to-many)) |
| `owned_by(field)` | rows belong to the account named in `field` (set by `field:owner`; see [Rows owned by accounts](accounts.md#rows-owned-by-accounts)) |
| `set_action(name, label, field, value)` | a bulk action setting a field on the selected rows |
| `action(name, label, \|ids\| async move { ... })` | a bulk action running your own code with the selected ids |

## Rich text

For text people format, such as a blog post or a product description, use an `html` field:

```sh
cadmus generate model Post title:string body:html
```

It is a long text column, with `ModelOptions::new().rich_text(["body"])` (which you can also
add to an existing `text` field by hand). In the admin it gets an editor
([Quill](https://quilljs.com), served by the admin itself) with headings, bold, italics,
underline, strike-through, lists, indentation, quotes, code blocks, links, images and
alignment. Without JavaScript the field is a plain box of HTML.

The column holds HTML, and the API sends and accepts it as a string:

```json
{ "title": "Hello", "body": "<h2>Hello</h2><p>A <strong>bold</strong> start.</p>" }
```

**Whatever is written is cleaned first**, through the admin, the API or a bulk action:
only the formatting above is kept. Scripts, `style` attributes, event handlers such as
`onclick`, forms, frames and `javascript:` or `data:` addresses are removed, and links get
`rel="noopener noreferrer"`. So it is safe to show a stored value as it is on your own
pages; in a template that escapes by default, mark it as safe (minijinja: `{{ post.body|safe }}`).
HTML that reaches the database some other way (an import, SQL by hand) is cleaned again
before the admin shows it, but not before your own pages do: run it through
`cadmus::clean_html` first.

Images are linked by address (`https://...`), not uploaded into the text. Lists show the
text of a rich-text field without its markup.

## Models without the generator

Any SeaORM entity can be registered with `.model::<your_entity::Entity>(...)`, including
ones you wrote by hand. Relations declared with `belongs_to` are picked up automatically.
Models must have a single-column primary key to be edited.

A resource that is not a database table at all can be served by implementing the `Resource`
trait and registering it with `App::resource`; see the
[API reference](https://docs.rs/cadmus).
