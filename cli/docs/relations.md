---
title: Relations
section: Guides
order: 130
summary: Links between models - foreign keys and many-to-many fields, how the admin and the API show them, and deleting linked records.
---

A link from one model to another (a foreign key) is a `references` field. Generate the model
being pointed to first:

```sh
cadmus generate model Author name:string email:string
cadmus generate model Post title:string author:references editor:references=author?
```

- `author:references` adds an `author_id` field pointing at `author`.
- `editor:references=author?` adds an optional `editor_id` that also points at `author`.
- A model can point to itself: `parent:references=category?` on `Category`.

Migrations create the database constraint that keeps the links valid.

## In the admin

- On forms, a link is a dropdown of the related records, by name.
- Lists show the related record's name, linked to it, instead of its number.
- A record is named by its first text field (`name`, `title`, ...). Choose another with
  `ModelOptions::new().display_field("email")`.
- Above 200 related records there is no dropdown; the form takes the id and says which
  record it currently points to.
- Names are shown only to accounts that may view the related model; others see and edit the
  plain number.
- `list_filter(["author_id"])` filters by the related records, by name.
- A record's page lists the records that point to it: an author's page shows their first
  posts, with **See all** (the post list filtered to that author) and **Add** (a new post
  already pointing to the author).

## In the API

The API reads and writes the id itself: `"author_id": 3`. Filter with `?author_id=3`.

## Many-to-many

A post can have many tags, and a tag many posts. Generate the model linked to first, then a
`many` field:

```sh
cadmus generate model Tag name:string
cadmus generate model Post title:string tags:many
```

`tags:many` links to the model `tag` (the field name without its last `s`); name another with
`readers:many=user`. The links are kept in a table of their own, `post_tags`, generated as
`src/models/post_tags.rs` and created by the next migration. Removing a post or a tag
removes its links.

- **In the admin**, the post form has a checkbox per tag. Add `"tags"` to `list_display` to
  show them in the list, and to `list_filter` for a dropdown of tags above it.
- **In the API**, the field is a list of ids: `"tags": [1, 3]`. Send a new list to replace
  the links (a `PATCH` that leaves `tags` out keeps them). Filter with `?tags=3`. Ids that do
  not exist are refused with `422`.

Without the generator, write the link table as an entity with a `belongs_to` relation to
each side, and register the field:

```rust
ModelOptions::new().many_to_many::<post_tags::Entity>(
    "tags",
    post_tags::Column::PostId,
    post_tags::Column::TagId,
)
```

## Deleting

A record that others still point to cannot be deleted: the admin says so, and the API
answers `409`. "Delete selected" keeps such rows and says how many.

## Hand-written relations

Any `belongs_to` relation in an entity is picked up the same way:

```rust
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::author::Entity",
        from = "Column::AuthorId",
        to = "super::author::Column::Id",
        on_delete = "Cascade"
    )]
    Author,
}
```

`on_delete = "Cascade"` deletes a post when its author is deleted, instead of refusing.
