//! The `Resource` seam: what the admin needs from a model, independent of the ORM.

use std::collections::HashMap;

use async_trait::async_trait;
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
