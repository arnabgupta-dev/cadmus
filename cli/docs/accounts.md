---
title: Accounts and permissions
section: Guides
order: 80
summary: Users, groups, superusers and staff, and the view/add/change/delete permissions that apply to the admin and the API.
---

Accounts are built in: they live in the `auth_user` table, and superusers manage them in the
admin under **Users** and **Groups**.

## The three switches

| Switch | Meaning |
|---|---|
| Active | Can log in. Untick to lock someone out without deleting the account. |
| Staff | Can use the admin. Without it, the account logs in through the REST API only. |
| Superuser | Can do everything, including managing accounts. |

An account that is not staff gets `403` on every admin page, whatever it logs in with, but
logs in through the API (password, providers, login links, API keys) with its permissions as
usual. That is how the users of an app built on the API get accounts: see
[Logins for apps](app-logins.md). To lock someone out completely, untick **Active**.

## Permissions

Staff who are not superusers can only do what they were given: for each model, any of
**view**, **add**, **change** and **delete**. Permissions are given to an account directly,
or to a **group** of accounts (such as "Editors"); an account has both. Whoever may change a
model may also view it, and file uploads count as a model named `files`.

The same rules apply in the admin and the API (`403` with code `forbidden`), and the admin
hides links and buttons an account cannot use.

### Rows owned by accounts

Permissions are per model. To let accounts see and change only **their own** rows, give the
model an `owner` field:

```sh
cadmus generate model Note text:text owner:owner
```

This adds a text field holding a username and `ModelOptions::new().owned_by("owner")`. Then,
for every account that is not a superuser, in the admin and the API:

- new rows belong to the account that adds them, whatever it sends;
- lists, counts, filters and dropdowns show only its own rows;
- other accounts' rows answer "not found", and cannot be changed, deleted or taken over.

Superusers see every row and can give a row to another account by changing its owner. The
account still needs the model's permissions (view, add, change, delete) as usual.

- Changes apply at once: permissions are checked on every request, so deactivating an
  account locks it out immediately, even if it is logged in.
- Nobody can remove their own access or delete their own account, so a superuser always
  remains. If you are locked out anyway, `cargo run -- promote --username U` makes an
  account superuser again.

## Account details

Accounts have a username, an e-mail address (unique, stored in lower case; it can be used to
log in), first and last name and phone, and record when they joined and last logged in.
Passwords are stored as Argon2id hashes, never as text.

For fields of your own, add a model linked to `cadmus::user::Entity`, with one profile row
per account:

```rust
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub user_id: i32,
    pub bio: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(belongs_to = "cadmus::user::Entity", from = "Column::UserId", to = "cadmus::user::Column::Id")]
    User,
}
```

## Creating accounts

- **From the terminal:** `cargo run -- createsuperuser --username U [--email E]`.
- **In the admin:** Users → Add. "Send an invitation instead" e-mails a link where the person
  chooses their own password (see [E-mail](email.md)).
- **By logging in through another site,** when sign-up is allowed (see
  [Login methods](login-methods.md)).

## Commands

| Command | What it does |
|---|---|
| `createsuperuser --username U [--email E] [--password-stdin]` | create a superuser |
| `changepassword --username U [--password-stdin]` | change a password |
| `promote --username U` | make an existing account an active superuser |
| `totp --username U [--disable]` | turn two-factor login on (prints a QR code) or off |
| `apikey --username U --name N [--days D] [--read-only]` | make an API key |

## Your own account store

To use accounts kept elsewhere, implement the `UserStore` trait and pass it to `App::users`.
The Users and Groups pages are then not shown.

Say what each account may do in `UserStore::access`, which is asked on every request.
Without it, accounts can log in but do nothing (and a warning is logged once):

```rust
async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
    Ok(match my_lookup(username).await? {
        Some(account) if account.is_admin => Some(Access::superuser()),
        Some(_) => Some(Access::none().grant("post", Action::View)),
        None => None, // may no longer log in
    })
}
```

API keys work only if you implement `UserStore::api_key`. Logins end when the
`password_hash` that `UserStore::find` returns changes, as with the built-in accounts.
