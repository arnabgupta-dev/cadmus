---
title: The admin site
section: Guides
order: 50
summary: The /admin site - dashboard, lists, search, filters, bulk actions, forms, and changing its look.
---

`/admin` is where people manage the data in a browser. It works on phones as well as
desktops, has light and dark themes, and shows each account only what it may do.

## Dashboard

The first page after logging in lists the models the account can see, with how many records
each has and what the account may do with them (view, add, change, delete, spelled out
rather than shown by colour alone). Superusers also see account totals, and everyone sees
whether their two-factor login is on.

## Lists

Each model's list page has:

- **search** across the model's `search_fields`;
- **sorting** by clicking a column heading;
- **pages** of results (25 by default; `ModelOptions::per_page` or `admin.page_size`);
- on phones, each row becomes a card.

<kbd>/</kbd> jumps to the search box, and every control can be reached with the keyboard.

## Filters

`ModelOptions::list_filter(["published", "author_id", "created_at"])` adds a dropdown per
field above the list:

| Field type | Choices |
|---|---|
| true/false | Yes, No |
| link to another model | the related records, by name |
| date or date-time | Today, Past 7 days, Past 30 days, This month, This year |
| anything else | the values present |
| optional fields | also "(empty)" |

Filters combine with search, and sorting and paging keep them. A link someone kept from
before, with a filter that no longer exists, shows nothing rather than failing.

## Acting on several rows

Rows can be ticked one by one, or all on the page, and acted on together:

- **Delete selected** is offered to accounts that may delete. It asks first, and keeps rows
  that other rows still point to, saying how many.
- **Setting a field:** `set_action("publish", "Mark as published", "published", "true")`
  sets `published` to true on every selected row, as editing them would.
- **Your own code:** `action(name, label, |ids| async move { ... })` runs with the selected
  primary keys:

```rust
ModelOptions::new().action("archive", "Archive", move |ids| {
    let db = db.clone();
    async move {
        post::Entity::update_many()
            .col_expr(post::Column::Archived, Expr::value(true))
            .filter(post::Column::Id.is_in(ids))
            .exec(&db)
            .await?;
        Ok(())
    }
})
```

Actions need the change permission, and at most 1000 rows are done at a time.

## Forms

- Buttons appear only for what the account may do; a record it cannot change opens
  read-only.
- Deleting asks for confirmation; saving confirms with a short notice.
- Links to other models are dropdowns of the related records (see [Relations](relations.md)).
- File fields have a file picker, a preview for images, and "Remove the file" (see
  [File uploads](file-uploads.md)).
- Invalid input is shown next to the field, and nothing is saved.

## Accounts pages

Superusers also get **Users** and **Groups** in the menu, to manage accounts and their
permissions (see [Accounts and permissions](accounts.md)). Every account has its own pages,
under the buttons by its name: two-factor login, password, and API keys.

## Changing the look

In the `admin` section of [Settings](settings.md):

| Setting | What it changes |
|---|---|
| `site_title` | the name in the header and the browser tab |
| `accent_color` | the colour of buttons and links, e.g. `"#0a7f5a"` |
| `logo_url` | a logo instead of the initial in the corner |
| `theme` | `Theme::Auto` (default: follows the device), `Theme::Light` or `Theme::Dark` |
| `custom_css_url` | your own stylesheet, loaded after the built-in one |
| `footer_text` | a line at the bottom of every page |
| `nav_links` | extra links in the menu |
| `web_fonts` | `false` uses the system typeface instead of loading Fira Sans from Google Fonts |
| `template_dir` | a folder of templates that replace the built-in ones by name |
| `path` | where the admin lives (default `/admin`) |
