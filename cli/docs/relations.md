---
title: Relations
section: Guides
order: 130
summary: Links between models (foreign keys) - generating them, how the admin shows them, and deleting linked records.
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

## In the API

The API reads and writes the id itself: `"author_id": 3`. Filter with `?author_id=3`.

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
