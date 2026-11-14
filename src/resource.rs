//! The `Resource` seam: what the admin needs from a model, independent of the ORM.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::error::AdminError;

/// One database row as a JSON object keyed by column name.
pub type Row = serde_json::Map<String, serde_json::Value>;

/// Values to write, keyed by column name. `None` is an explicit null; a missing key means
/// "not provided".
pub type Input = HashMap<String, Option<String>>;

/// Raw HTML form submission.
pub(crate) type FormData = HashMap<String, String>;

/// How [`Resource::update`] treats columns that are missing from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Every writable column must be provided (or be nullable): HTML forms and HTTP `PUT`.
    Replace,
    /// Missing columns keep their current value: HTTP `PATCH`.
    Patch,
}

/// How a field is rendered and parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// Single-line text.
    Text,
    /// Multi-line text.
    LongText,
    /// Whole number.
    Integer,
    /// Floating-point number.
    Float,
    /// Checkbox.
    Boolean,
    /// Date and time.
    DateTime,
    /// Calendar date.
    Date,
    /// Time of day.
    Time,
    /// JSON document.
    Json,
    /// A type the admin can display but not edit.
    Other,
}

/// Metadata for one column of a model.
#[derive(Debug, Clone, Serialize)]
pub struct Field {
    /// Column name (also the key in [`Row`] and [`Input`]).
    pub name: String,
    /// Label shown in the UI.
    pub label: String,
    /// Rendering/parsing kind.
    pub kind: FieldKind,
    /// Whether the column accepts NULL.
    pub nullable: bool,
    /// Whether the column is the primary key.
    pub primary_key: bool,
    /// Whether the database generates the value (auto-increment primary key).
    pub auto: bool,
    /// Whether the admin can write this column.
    pub editable: bool,
    /// Whether the database fills the column in when a new row leaves it out.
    pub has_default: bool,
    /// The row of another model this column points to, if it is a foreign key.
    pub reference: Option<Reference>,
}

/// What a foreign-key column points to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reference {
    /// URL name ([`Resource::slug`]) of the model pointed to.
    pub model: String,
    /// The column of that model holding the value, normally its primary key.
    pub column: String,
}

type ActionFn =
    Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, Result<(), AdminError>> + Send + Sync>;

/// What a bulk action does to the selected rows.
#[derive(Clone)]
pub(crate) enum ActionKind {
    /// Sets one column to one value, as an edit would.
    Set { field: String, value: String },
    /// Runs your code with the selected primary keys.
    Custom(ActionFn),
}

/// An action offered for the rows selected on a list page.
#[derive(Clone)]
pub(crate) struct BulkAction {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) kind: ActionKind,
}

impl std::fmt::Debug for BulkAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkAction")
            .field("name", &self.name)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// Per-model UI customisation, in the spirit of Django's `ModelAdmin`.
#[derive(Debug, Clone, Default)]
pub struct ModelOptions {
    pub(crate) label: Option<String>,
    pub(crate) list_display: Vec<String>,
    pub(crate) search_fields: Vec<String>,
    pub(crate) readonly_fields: Vec<String>,
    pub(crate) hidden_fields: Vec<String>,
    pub(crate) per_page: Option<u64>,
    pub(crate) display_field: Option<String>,
    pub(crate) list_filter: Vec<String>,
    pub(crate) actions: Vec<BulkAction>,
    pub(crate) file_fields: Vec<String>,
}

impl ModelOptions {
    /// Starts from the defaults: every column listed, every text column searchable.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the name shown in the navigation.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Columns shown on the list page, in order.
    #[must_use]
    pub fn list_display<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.list_display = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Columns matched by the search box.
    #[must_use]
    pub fn search_fields<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.search_fields = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Columns shown on forms but never written.
    #[must_use]
    pub fn readonly_fields<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.readonly_fields = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Columns never shown or returned (e.g. password hashes).
    #[must_use]
    pub fn hidden_fields<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hidden_fields = columns.into_iter().map(Into::into).collect();
        self
    }

    /// The column that names a row wherever another model points to it: in the dropdown on
    /// that model's form and in its list. Defaults to the first text column, e.g. `name` or
    /// `title`.
    #[must_use]
    pub fn display_field(mut self, column: impl Into<String>) -> Self {
        self.display_field = Some(column.into());
        self
    }

    /// Columns the list page offers filters for, in order, as Django's `list_filter`: yes/no
    /// for booleans, the related rows for foreign keys, periods such as "past 7 days" for
    /// dates, and the values present for other columns.
    #[must_use]
    pub fn list_filter<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.list_filter = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Offers an action for the rows selected on the list page that sets `field` to `value`
    /// in each, as editing them would: e.g. `set_action("publish", "Mark as published",
    /// "published", "true")`. Needs the `change` permission. "Delete selected" is always
    /// offered to accounts that may delete.
    #[must_use]
    pub fn set_action(
        mut self,
        name: impl Into<String>,
        label: impl Into<String>,
        field: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.actions.push(BulkAction {
            name: name.into(),
            label: label.into(),
            kind: ActionKind::Set {
                field: field.into(),
                value: value.into(),
            },
        });
        self
    }

    /// Offers an action for the rows selected on the list page that runs `run` with their
    /// primary keys. Needs the `change` permission. Capture what it needs, such as a
    /// database connection:
    ///
    /// ```ignore
    /// let db = app.db().clone();
    /// ModelOptions::new().action("archive", "Archive", move |ids| {
    ///     let db = db.clone();
    ///     async move {
    ///         post::Entity::update_many()
    ///             .col_expr(post::Column::Archived, Expr::value(true))
    ///             .filter(post::Column::Id.is_in(ids.iter().filter_map(|id| id.parse::<i32>().ok())))
    ///             .exec(&db)
    ///             .await?;
    ///         Ok(())
    ///     }
    /// })
    /// ```
    #[must_use]
    pub fn action<F, Fut>(
        mut self,
        name: impl Into<String>,
        label: impl Into<String>,
        run: F,
    ) -> Self
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AdminError>> + Send + 'static,
    {
        let run: ActionFn = Arc::new(move |ids| Box::pin(run(ids)));
        self.actions.push(BulkAction {
            name: name.into(),
            label: label.into(),
            kind: ActionKind::Custom(run),
        });
        self
    }

    /// Integer columns that hold an uploaded file's id: the form shows a file picker (with
    /// the current file, a preview for images, and "Remove" when the column is nullable),
    /// and the list shows the file's name. Needs file uploads (`uploads.storage_url`). API
    /// clients upload to `/api/files` and send the returned id.
    #[must_use]
    pub fn file_fields<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.file_fields = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Rows per list page for this model (overrides the global setting).
    #[must_use]
    pub fn per_page(mut self, per_page: u64) -> Self {
        self.per_page = Some(per_page.max(1));
        self
    }
}

/// A page request for [`Resource::list`].
#[derive(Debug, Clone)]
pub struct ListQuery {
    /// Zero-based page index.
    pub page: u64,
    /// Rows per page.
    pub per_page: u64,
    /// Free-text search, if any.
    pub search: Option<String>,
    /// Column to order by; by primary key if `None`. Callers only pass visible columns.
    pub sort: Option<String>,
    /// Whether `sort` runs from highest to lowest.
    pub descending: bool,
    /// Conditions every row must meet. Callers only pass visible, filterable columns.
    pub filters: Vec<Filter>,
}

/// How a [`Filter`] compares the column with its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// Equal to the value.
    Equal,
    /// Greater than the value.
    Greater,
    /// Greater than or equal to the value.
    GreaterOrEqual,
    /// Less than the value.
    Less,
    /// Less than or equal to the value.
    LessOrEqual,
    /// Empty (NULL); the value is unused.
    IsNull,
    /// Not empty; the value is unused.
    IsNotNull,
}

/// One condition on a column, as `?published=true` or `?created_at__gte=2026-01-01` asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Column name.
    pub field: String,
    /// The comparison.
    pub op: FilterOp,
    /// The value as text, parsed like form input for the column's type.
    pub value: String,
}

impl ListQuery {
    /// The first `per_page` rows, in primary-key order.
    #[must_use]
    pub fn first(per_page: u64) -> Self {
        Self {
            page: 0,
            per_page,
            search: None,
            sort: None,
            descending: false,
            filters: Vec::new(),
        }
    }
}

/// One page of rows plus the total match count.
#[derive(Debug, Clone, Serialize)]
pub struct Page {
    /// Rows on this page.
    pub rows: Vec<Row>,
    /// Total rows matching the query.
    pub total: u64,
}

/// A model the admin can list and edit.
///
/// [`crate::SeaOrmResource`] implements this for any SeaORM entity. Implement it yourself to
/// expose something else (another ORM, a remote API) through the same UI.
#[async_trait]
pub trait Resource: Send + Sync {
    /// URL segment, unique per admin.
    fn slug(&self) -> &str;
    /// Name shown in the UI.
    fn label(&self) -> &str;
    /// Column metadata, in display order.
    fn fields(&self) -> &[Field];
    /// UI options for this model.
    fn options(&self) -> &ModelOptions;

    /// Returns one page of rows.
    async fn list(&self, query: &ListQuery) -> Result<Page, AdminError>;
    /// Returns the row with primary key `id`, if it exists.
    async fn get(&self, id: &str) -> Result<Option<Row>, AdminError>;
    /// Returns the rows with these primary keys, in any order, leaving out those that do not
    /// exist. Used to show what foreign keys point to.
    ///
    /// The default asks for them one by one; override it if they can be fetched together.
    async fn get_many(&self, ids: &[String]) -> Result<Vec<Row>, AdminError> {
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            rows.extend(self.get(id).await?);
        }
        Ok(rows)
    }
    /// Up to `limit` distinct non-empty values of column `field`, in order, for a list filter.
    /// The default offers none, so the filter is left out.
    async fn distinct(&self, _field: &str, _limit: u64) -> Result<Vec<String>, AdminError> {
        Ok(Vec::new())
    }
    /// Inserts a row and returns it as stored (`None` if it cannot be read back by id).
    async fn create(&self, input: &Input) -> Result<Option<Row>, AdminError>;
    /// Updates the row with primary key `id`.
    async fn update(&self, id: &str, input: &Input, mode: WriteMode) -> Result<(), AdminError>;
    /// Deletes the row with primary key `id`.
    async fn delete(&self, id: &str) -> Result<(), AdminError>;
}

/// Turns `created_at` into `Created at`.
pub(crate) fn humanize(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    }
}
