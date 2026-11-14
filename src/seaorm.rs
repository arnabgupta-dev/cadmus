//! [`Resource`] for any SeaORM entity, built from the entity's own column metadata.

use std::collections::HashMap;
use std::fmt::Display;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sea_orm::prelude::{Decimal, Uuid};
use sea_orm::schema::SchemaBuilder;
use sea_orm::sea_query::{
    Alias, ArrayType, ColumnType, Expr, Func, Query, SelectStatement, SimpleExpr,
};
use sea_orm::{
    ActiveModelBehavior, ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait,
    DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, EntityTrait, IdenStatic, Identity,
    IntoActiveModel, Iterable, ModelTrait, PaginatorTrait, PrimaryKeyToColumn, PrimaryKeyTrait,
    QueryFilter, QueryOrder, QuerySelect, RelationTrait, TransactionTrait, Value,
};

use crate::error::AdminError;
use crate::migrations::EntityTable;
use crate::resource::{
    Field, FieldKind, Filter, FilterOp, Input, LinkField, ListQuery, ManyToMany, ModelOptions,
    Page, Reference, Resource, Row, WriteMode, humanize,
};

/// Marks a [`DbErr`] made by [`invalid`].
const INVALID: &str = "cadmus:invalid\u{0}";

/// An error for a save hook to return when a value is not acceptable. The admin shows the
/// message next to the form, and the API answers `422` with it.
///
/// ```ignore
/// #[async_trait::async_trait]
/// impl ActiveModelBehavior for ActiveModel {
///     async fn before_save<C: ConnectionTrait>(self, _db: &C, _insert: bool) -> Result<Self, DbErr> {
///         if matches!(self.price, Set(p) if p < 0.0) {
///             return Err(cadmus::invalid("price", "must not be negative"));
///         }
///         Ok(self)
///     }
/// }
/// ```
#[must_use]
pub fn invalid(field: impl AsRef<str>, message: impl AsRef<str>) -> DbErr {
    DbErr::Custom(format!(
        "{INVALID}{}\u{0}{}",
        field.as_ref(),
        message.as_ref()
    ))
}

/// A database error as the caller should see it: a hook's [`invalid`] becomes a field error.
fn db_error(error: DbErr) -> AdminError {
    if let DbErr::Custom(text) = &error
        && let Some(rest) = text.strip_prefix(INVALID)
    {
        let (field, message) = rest.split_once('\u{0}').unwrap_or(("", rest));
        return AdminError::BadInput {
            field: field.to_owned(),
            message: message.to_owned(),
        };
    }
    error.into()
}

/// Admin resource backed by a SeaORM entity.
///
/// Columns, types, nullability and the primary key are read from the entity, so registering a
/// model needs no extra code:
///
/// ```ignore
/// let posts = SeaOrmResource::<post::Entity>::new(db.clone());
/// ```
pub struct SeaOrmResource<E: EntityTrait> {
    db: DatabaseConnection,
    slug: String,
    label: String,
    fields: Vec<Field>,
    /// Parallel to `fields`.
    columns: Vec<E::Column>,
    /// Parallel to `fields`.
    types: Vec<ColumnType>,
    /// Parallel to `fields`: whether the entity declares a default value for the column.
    has_default: Vec<bool>,
    /// Index of the single primary-key column; `None` for composite or missing keys.
    pk: Option<usize>,
    options: ModelOptions,
}

impl<E: EntityTrait> SeaOrmResource<E> {
    /// Introspects `E` and binds it to `db`.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        let table = E::default().table_name().to_owned();
        let pk_names: Vec<&'static str> = E::PrimaryKey::iter()
            .map(|k| k.into_column().as_str())
            .collect();
        let auto = <E::PrimaryKey as PrimaryKeyTrait>::auto_increment();

        let references = references::<E>(&table);
        let mut fields = Vec::new();
        let mut columns = Vec::new();
        let mut types = Vec::new();
        let mut has_default = Vec::new();
        for column in E::Column::iter() {
            let def = column.def();
            let ty = def.get_column_type().clone();
            let name = column.as_str();
            let primary_key = pk_names.contains(&name);
            let kind = kind_of(&ty);
            let reference = references
                .iter()
                .find(|(column, _)| column == name)
                .map(|(_, reference)| reference.clone());
            // `author_id` pointing at an author is labelled "Author", not "Author id".
            let label = match (&reference, name.strip_suffix("_id")) {
                (Some(_), Some(stem)) if !stem.is_empty() => humanize(stem),
                _ => humanize(name),
            };
            fields.push(Field {
                name: name.to_owned(),
                label,
                kind,
                nullable: def.is_null(),
                primary_key,
                auto: primary_key && auto,
                editable: kind != FieldKind::Other,
                has_default: def.get_column_default().is_some(),
                reference,
                choices: match &ty {
                    ColumnType::Enum { variants, .. } => {
                        variants.iter().map(|v| v.to_string()).collect()
                    }
                    _ => Vec::new(),
                },
            });
            columns.push(column);
            types.push(ty);
            has_default.push(def.get_column_default().is_some());
        }
        let pk = match pk_names.as_slice() {
            [only] => fields.iter().position(|f| f.name == *only),
            _ => None,
        };
        Self {
            db,
            label: humanize(&table),
            slug: table,
            fields,
            columns,
            types,
            has_default,
            pk,
            options: ModelOptions::default(),
        }
    }

    /// Applies per-model UI options.
    #[must_use]
    pub fn options(mut self, options: ModelOptions) -> Self {
        if let Some(label) = &options.label {
            self.label.clone_from(label);
        }
        for (column, values) in &options.choices {
            if let Some(field) = self.fields.iter_mut().find(|f| &f.name == column) {
                field.kind = FieldKind::Choice;
                field.choices.clone_from(values);
            }
        }
        self.options = options;
        self
    }

    fn link(&self, name: &str) -> Option<&ManyToMany> {
        self.options.many_to_many.iter().find(|m| m.name == name)
    }

    /// Adds each many-to-many field to these rows, as a list of ids.
    async fn with_links(&self, mut rows: Vec<Row>) -> Result<Vec<Row>, AdminError> {
        let (Some(pk), false) = (self.pk, self.options.many_to_many.is_empty()) else {
            return Ok(rows);
        };
        let pk = &self.fields[pk].name;
        let ids: Vec<String> = rows.iter().map(|row| text_of(row.get(pk))).collect();
        for many in &self.options.many_to_many {
            let mut linked: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for (owner, value) in many.link.pairs(&self.db, &ids).await? {
                linked.entry(owner).or_default().push(value);
            }
            for row in &mut rows {
                let found = linked.remove(&text_of(row.get(pk))).unwrap_or_default();
                row.insert(many.name.clone(), serde_json::Value::Array(found));
            }
        }
        Ok(rows)
    }

    /// The many-to-many values in `input`, checked: each field's ids, all existing.
    async fn link_input(
        &self,
        txn: &DatabaseTransaction,
        input: &Input,
    ) -> Result<Vec<(&ManyToMany, Vec<String>)>, AdminError> {
        let mut out = Vec::new();
        for many in &self.options.many_to_many {
            let Some(raw) = input.get(&many.name) else {
                continue;
            };
            let bad = |message: String| AdminError::BadInput {
                field: many.name.clone(),
                message,
            };
            let ids = match raw.as_deref().map(str::trim) {
                None | Some("") => Vec::new(),
                Some(text) => parse_ids(text).map_err(bad)?,
            };
            let missing = many.link.missing(txn, &ids).await?;
            if !missing.is_empty() {
                return Err(bad(format!(
                    "there is no row with the id {}",
                    missing.join(", ")
                )));
            }
            out.push((many, ids));
        }
        Ok(out)
    }

    /// The SQL condition for one list filter.
    fn condition(&self, filter: &Filter) -> Result<SimpleExpr, AdminError> {
        if let Some(many) = self.link(&filter.field) {
            // Rows linked to the given row of the other model.
            let pk = self.pk_index()?;
            let linked = many.link.owners_linked_to(&filter.value)?;
            return Ok(match filter.op {
                FilterOp::Equal => self.columns[pk].in_subquery(linked),
                _ => {
                    return Err(AdminError::BadInput {
                        field: filter.field.clone(),
                        message: "only `=` works for a many-to-many field".into(),
                    });
                }
            });
        }
        let i = self
            .fields
            .iter()
            .position(|f| f.name == filter.field)
            .ok_or_else(|| AdminError::BadInput {
                field: filter.field.clone(),
                message: "no such column".into(),
            })?;
        let column = self.columns[i];
        let value = || {
            parse_value(&self.types[i], Some(&filter.value)).map_err(|message| {
                AdminError::BadInput {
                    field: filter.field.clone(),
                    message: format!("`{}` is not a valid value: {message}", filter.value),
                }
            })
        };
        Ok(match filter.op {
            FilterOp::Equal => column.eq(value()?),
            FilterOp::Greater => column.gt(value()?),
            FilterOp::GreaterOrEqual => column.gte(value()?),
            FilterOp::Less => column.lt(value()?),
            FilterOp::LessOrEqual => column.lte(value()?),
            FilterOp::IsNull => column.is_null(),
            FilterOp::IsNotNull => column.is_not_null(),
        })
    }

    /// Condition: column `i`, as text, contains `term`, ignoring case.
    ///
    /// Written with a substring function rather than `LIKE` so that the result is the same on
    /// every database: `LIKE` is case-sensitive on PostgreSQL but not on SQLite, and would
    /// treat `%` and `_` in the term as wildcards.
    fn matches(&self, i: usize, term: &str) -> SimpleExpr {
        // Imported here only: its `max`/`min` would otherwise shadow the integer ones.
        use sea_orm::sea_query::ExprTrait;

        let backend = self.db.get_database_backend();
        let column = Expr::col((E::default(), self.columns[i]));
        let text = if matches!(self.fields[i].kind, FieldKind::Text | FieldKind::LongText) {
            column
        } else {
            let text_type = if backend == DbBackend::MySql {
                "char"
            } else {
                "text"
            };
            column.cast_as(Alias::new(text_type))
        };
        let position = if backend == DbBackend::Postgres {
            "strpos"
        } else {
            "instr"
        };
        let found = Func::cust(Alias::new(position))
            .arg(Func::lower(text))
            .arg(Func::lower(Expr::val(term)));
        Expr::expr(found).gt(0)
    }

    /// Makes a row read from the database look the same whichever database it came from.
    ///
    /// Two generated types are printed differently by the drivers:
    /// - date-times: SQLite returns its stored text (`2026-03-04 05:06:07`), PostgreSQL an
    ///   ISO 8601 string;
    /// - JSON: SQLite returns the document as a string, PostgreSQL as the document itself.
    fn normalise(&self, mut row: Row) -> Row {
        let sqlite = self.db.get_database_backend() == DbBackend::Sqlite;
        for (field, ty) in self.fields.iter().zip(&self.types) {
            let Some(value) = row.get_mut(&field.name) else {
                continue;
            };
            // MySQL stores true/false as a small integer and reads it back as one.
            if field.kind == FieldKind::Boolean
                && let serde_json::Value::Number(n) = value
            {
                *value = serde_json::Value::Bool(n.as_i64() != Some(0));
                continue;
            }
            // Decimals are exact text everywhere, whatever the driver returned.
            if field.kind == FieldKind::Decimal {
                if let serde_json::Value::Number(n) = value {
                    *value = serde_json::Value::String(n.to_string());
                }
                continue;
            }
            let serde_json::Value::String(text) = value else {
                continue;
            };
            match field.kind {
                // Read as text on SQLite; see `reader`.
                FieldKind::Float => {
                    if let Some(n) = text
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                    {
                        *value = serde_json::Value::Number(n);
                    }
                }
                FieldKind::DateTime => {
                    if let Some(iso) = iso_datetime(ty, text) {
                        *text = iso;
                    }
                }
                // Elsewhere a string here is a JSON document that is itself a string.
                FieldKind::Json if sqlite => {
                    if let Ok(document) = serde_json::from_str(text) {
                        *value = document;
                    }
                }
                _ => {}
            }
        }
        row
    }

    /// The rows of `E`, ready to be read as JSON.
    ///
    /// On SQLite, floating-point and decimal columns are read as text: the driver's JSON
    /// conversion would otherwise take every `REAL` for a 32-bit float and lose digits
    /// (`9.95` read back as `9.949999809265137`). [`Self::normalise`] turns them back.
    fn reader(&self) -> sea_orm::Select<E> {
        let lossy = |kind: FieldKind| matches!(kind, FieldKind::Float | FieldKind::Decimal);
        if self.db.get_database_backend() != DbBackend::Sqlite
            || !self.fields.iter().any(|f| lossy(f.kind))
        {
            return E::find();
        }
        use sea_orm::sea_query::ExprTrait;
        let mut select = E::find().select_only();
        for (field, column) in self.fields.iter().zip(&self.columns) {
            select = if lossy(field.kind) {
                select.column_as(
                    Expr::col((E::default(), *column)).cast_as(Alias::new("text")),
                    column.as_str(),
                )
            } else {
                select.column(*column)
            };
        }
        select
    }

    fn pk_index(&self) -> Result<usize, AdminError> {
        self.pk.ok_or_else(|| {
            AdminError::Unsupported(format!(
                "`{}` needs a single-column primary key to be viewed or edited by id",
                self.slug
            ))
        })
    }

    fn pk_value(&self, id: &str) -> Result<(E::Column, Value), AdminError> {
        let i = self.pk_index()?;
        // An id that does not parse cannot name an existing row.
        let value = parse_value(&self.types[i], Some(id))
            .map_err(|_| AdminError::NotFound(format!("{} `{id}`", self.slug)))?;
        Ok((self.columns[i], value))
    }

    /// Fills `model` from the input, skipping columns the admin must not write.
    fn apply_input(
        &self,
        model: &mut E::ActiveModel,
        input: &Input,
        creating: bool,
        mode: WriteMode,
    ) -> Result<(), AdminError> {
        for (i, field) in self.fields.iter().enumerate() {
            let skip = !field.editable
                || field.auto
                || (field.primary_key && !creating)
                || self.options.readonly_fields.contains(&field.name)
                || self.options.hidden_fields.contains(&field.name);
            if skip {
                continue;
            }
            let bad = |message: String| AdminError::BadInput {
                field: field.name.clone(),
                message,
            };
            let ty = &self.types[i];
            let is_text = matches!(field.kind, FieldKind::Text | FieldKind::LongText);
            let generated;
            let provided = match input.get(&field.name) {
                None if mode == WriteMode::Patch => continue,
                // A new row with a UUID key and none given gets a fresh one.
                None | Some(None)
                    if creating && field.primary_key && field.kind == FieldKind::Uuid =>
                {
                    generated = Uuid::new_v4().to_string();
                    Some(generated.as_str())
                }
                Some(Some(text))
                    if creating
                        && field.primary_key
                        && field.kind == FieldKind::Uuid
                        && text.trim().is_empty() =>
                {
                    generated = Uuid::new_v4().to_string();
                    Some(generated.as_str())
                }
                // Let the database apply its declared default on insert.
                None if creating && (field.nullable || self.has_default[i]) => continue,
                None | Some(None) => None,
                // An empty box in a form means "no value" for everything except text.
                Some(Some(text)) if text.is_empty() && !is_text => None,
                Some(Some(text)) => Some(text.as_str()),
            };
            if let Some(text) = provided
                && !field.choices.is_empty()
                && !field.choices.iter().any(|c| c == text)
            {
                return Err(bad(format!("must be one of: {}", field.choices.join(", "))));
            }
            let value = match provided {
                Some(text) => parse_value(ty, Some(text)).map_err(bad)?,
                None if field.nullable => parse_value(ty, None).map_err(bad)?,
                None => return Err(bad("this field is required".to_owned())),
            };
            set_value::<E>(model, self.columns[i], value, ty).map_err(|e| bad(e.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait]
impl<E> Resource for SeaOrmResource<E>
where
    E: EntityTrait,
    E::Model: IntoActiveModel<E::ActiveModel>,
    E::ActiveModel: ActiveModelBehavior + Send,
{
    fn slug(&self) -> &str {
        &self.slug
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn options(&self) -> &ModelOptions {
        &self.options
    }

    fn links(&self) -> Vec<LinkField> {
        self.options
            .many_to_many
            .iter()
            .filter_map(|many| {
                Some(LinkField {
                    name: many.name.clone(),
                    label: many.label.clone(),
                    model: many.link.target()?.model.clone(),
                })
            })
            .collect()
    }

    async fn list(&self, query: &ListQuery) -> Result<Page, AdminError> {
        let mut select = self.reader();
        if let Some(term) = query.search.as_deref().filter(|t| !t.is_empty()) {
            let mut any = Condition::any();
            let mut searchable = 0;
            for (i, field) in self.fields.iter().enumerate() {
                let chosen = if self.options.search_fields.is_empty() {
                    matches!(field.kind, FieldKind::Text | FieldKind::LongText)
                } else {
                    self.options.search_fields.contains(&field.name)
                };
                if chosen && !self.options.hidden_fields.contains(&field.name) {
                    any = any.add(self.matches(i, term));
                    searchable += 1;
                }
            }
            if searchable > 0 {
                select = select.filter(any);
            }
        }
        for filter in &query.filters {
            select = select.filter(self.condition(filter)?);
        }
        let sorted = query.sort.as_deref().and_then(|name| {
            self.fields
                .iter()
                .position(|f| f.name == name && !self.options.hidden_fields.contains(&f.name))
        });
        if let Some(i) = sorted {
            let order = if query.descending {
                sea_orm::Order::Desc
            } else {
                sea_orm::Order::Asc
            };
            select = select.order_by(self.columns[i], order);
        }
        // The primary key breaks ties, so pages never overlap or skip rows.
        if let Some(i) = self.pk.filter(|i| Some(*i) != sorted) {
            select = select.order_by_asc(self.columns[i]);
        }
        let paginator = select.into_json().paginate(&self.db, query.per_page.max(1));
        let total = paginator.num_items().await?;
        let rows = paginator
            .fetch_page(query.page)
            .await?
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::Object(map) => Some(self.normalise(map)),
                _ => None,
            })
            .collect();
        let rows = self.with_links(rows).await?;
        Ok(Page { rows, total })
    }

    async fn get(&self, id: &str) -> Result<Option<Row>, AdminError> {
        let (column, value) = match self.pk_value(id) {
            Ok(pair) => pair,
            Err(AdminError::NotFound(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        let row = self
            .reader()
            .filter(column.eq(value))
            .into_json()
            .one(&self.db)
            .await?;
        let Some(serde_json::Value::Object(map)) = row else {
            return Ok(None);
        };
        Ok(self.with_links(vec![self.normalise(map)]).await?.pop())
    }

    async fn get_many(&self, ids: &[String]) -> Result<Vec<Row>, AdminError> {
        let i = self.pk_index()?;
        // An id that does not parse cannot name an existing row.
        let values: Vec<Value> = ids
            .iter()
            .filter_map(|id| parse_value(&self.types[i], Some(id)).ok())
            .collect();
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .reader()
            .filter(self.columns[i].is_in(values))
            .into_json()
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::Object(map) => Some(self.normalise(map)),
                _ => None,
            })
            .collect())
    }

    async fn distinct(&self, field: &str, limit: u64) -> Result<Vec<String>, AdminError> {
        let Some(i) = self.fields.iter().position(|f| f.name == field) else {
            return Ok(Vec::new());
        };
        let column = self.columns[i];
        let values = E::find()
            .select_only()
            .column(column)
            .distinct()
            .filter(column.is_not_null())
            .order_by_asc(column)
            .limit(limit)
            .into_json()
            .all(&self.db)
            .await?;
        Ok(values
            .into_iter()
            .filter_map(|value| match value {
                serde_json::Value::Object(map) => {
                    let row = self.normalise(map);
                    row.get(field).map(|v| match v {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                }
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect())
    }

    async fn create(&self, input: &Input) -> Result<Option<Row>, AdminError> {
        let mut model = <E::ActiveModel as ActiveModelTrait>::default();
        self.apply_input(&mut model, input, true, WriteMode::Replace)?;
        // One transaction, so a row is never left without the links it was saved with.
        let txn = self.db.begin().await?;
        let links = self.link_input(&txn, input).await?;
        let saved = model.insert(&txn).await.map_err(db_error)?;
        let id = self.pk.and_then(|i| id_text(&saved.get(self.columns[i])));
        if let Some(id) = &id {
            for (many, targets) in &links {
                many.link.replace(&txn, id, targets).await?;
            }
        }
        txn.commit().await?;
        match id {
            Some(id) => self.get(&id).await,
            None => Ok(None),
        }
    }

    async fn update(&self, id: &str, input: &Input, mode: WriteMode) -> Result<(), AdminError> {
        let (column, value) = self.pk_value(id)?;
        let txn = self.db.begin().await?;
        // The whole record, so save hooks see every value, not only the changed ones.
        let current = E::find()
            .filter(column.eq(value))
            .one(&txn)
            .await?
            .ok_or_else(|| AdminError::NotFound(format!("{} `{id}`", self.slug)))?;
        let mut model = current.into_active_model();
        self.apply_input(&mut model, input, false, mode)?;
        let links = self.link_input(&txn, input).await?;
        if model.is_changed() {
            model.update(&txn).await.map_err(db_error)?;
        }
        for (many, targets) in &links {
            many.link.replace(&txn, id, targets).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), AdminError> {
        let (column, value) = self.pk_value(id)?;
        let txn = self.db.begin().await?;
        let current = E::find()
            .filter(column.eq(value))
            .one(&txn)
            .await?
            .ok_or_else(|| AdminError::NotFound(format!("{} `{id}`", self.slug)))?;
        for many in &self.options.many_to_many {
            many.link.clear(&txn, id).await?;
        }
        // Through the model, so its delete hooks run.
        current
            .into_active_model()
            .delete(&txn)
            .await
            .map_err(db_error)?;
        txn.commit().await?;
        Ok(())
    }
}

/// Sets a column, retrying a timestamp-with-zone as UTC.
///
/// SeaORM maps both `DateTime<Utc>` and `DateTime<FixedOffset>` fields to the same column type,
/// and the column metadata does not say which one the model uses.
fn set_value<E: EntityTrait>(
    model: &mut E::ActiveModel,
    column: E::Column,
    value: Value,
    ty: &ColumnType,
) -> Result<(), DbErr> {
    if let (ColumnType::TimestampWithTimeZone, Value::ChronoDateTimeWithTimeZone(inner)) =
        (ty, &value)
    {
        let utc = Value::ChronoDateTimeUtc(inner.map(|d| d.with_timezone(&Utc)));
        if model.try_set(column, value).is_ok() {
            return Ok(());
        }
        return model.try_set(column, utc);
    }
    model.try_set(column, value)
}

/// The foreign keys of `E`, read from its `Relation` enum: each column of `table` that a
/// `belongs_to` relation uses, with what it points to.
///
/// Relations declared from the other side (`has_many`, `has_one`) and foreign keys of more
/// than one column are left out.
fn references<E: EntityTrait>(table: &str) -> Vec<(String, Reference)> {
    E::Relation::iter()
        .filter_map(|relation| {
            let def = relation.def();
            let owns_key = !def.is_owner && def.from_tbl.sea_orm_table().inner() == table;
            match (owns_key, &def.from_col, &def.to_col) {
                (true, Identity::Unary(from), Identity::Unary(to)) => Some((
                    from.inner().into_owned(),
                    Reference {
                        model: def.to_tbl.sea_orm_table().inner().into_owned(),
                        column: to.inner().into_owned(),
                    },
                )),
                _ => None,
            }
        })
        .collect()
}

/// Primary-key value as it appears in a URL.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::TinyInt(Some(n)) => Some(n.to_string()),
        Value::SmallInt(Some(n)) => Some(n.to_string()),
        Value::Int(Some(n)) => Some(n.to_string()),
        Value::BigInt(Some(n)) => Some(n.to_string()),
        Value::TinyUnsigned(Some(n)) => Some(n.to_string()),
        Value::SmallUnsigned(Some(n)) => Some(n.to_string()),
        Value::Unsigned(Some(n)) => Some(n.to_string()),
        Value::BigUnsigned(Some(n)) => Some(n.to_string()),
        Value::String(Some(text)) => Some(text.clone()),
        Value::Uuid(Some(id)) => Some(id.to_string()),
        _ => None,
    }
}

fn boolean(text: &str) -> Result<bool, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        other => Err(format!("`{other}` is not a boolean")),
    }
}

fn kind_of(ty: &ColumnType) -> FieldKind {
    match ty {
        ColumnType::Char(_) | ColumnType::String(_) => FieldKind::Text,
        ColumnType::Text => FieldKind::LongText,
        ColumnType::TinyInteger
        | ColumnType::SmallInteger
        | ColumnType::Integer
        | ColumnType::BigInteger
        | ColumnType::TinyUnsigned
        | ColumnType::SmallUnsigned
        | ColumnType::Unsigned
        | ColumnType::BigUnsigned => FieldKind::Integer,
        ColumnType::Float | ColumnType::Double => FieldKind::Float,
        ColumnType::Boolean => FieldKind::Boolean,
        ColumnType::DateTime | ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => {
            FieldKind::DateTime
        }
        ColumnType::Date => FieldKind::Date,
        ColumnType::Time => FieldKind::Time,
        ColumnType::Json | ColumnType::JsonBinary => FieldKind::Json,
        ColumnType::Uuid => FieldKind::Uuid,
        ColumnType::Decimal(_) | ColumnType::Money(_) => FieldKind::Decimal,
        ColumnType::Enum { .. } => FieldKind::Choice,
        ColumnType::Array(inner) if array_type(inner).is_some() => FieldKind::List,
        _ => FieldKind::Other,
    }
}

/// The element type of an editable array column.
fn array_type(inner: &ColumnType) -> Option<ArrayType> {
    Some(match inner {
        ColumnType::Char(_) | ColumnType::String(_) | ColumnType::Text => ArrayType::String,
        ColumnType::SmallInteger => ArrayType::SmallInt,
        ColumnType::Integer => ArrayType::Int,
        ColumnType::BigInteger => ArrayType::BigInt,
        ColumnType::Float => ArrayType::Float,
        ColumnType::Double => ArrayType::Double,
        ColumnType::Boolean => ArrayType::Bool,
        ColumnType::Uuid => ArrayType::Uuid,
        ColumnType::Date => ArrayType::ChronoDate,
        _ => return None,
    })
}

/// A list typed into a form or sent to the API: a JSON array, or one value per line.
fn list_items(text: &str) -> Result<Vec<String>, String> {
    let text = text.trim();
    if text.starts_with('[') {
        let items: Vec<serde_json::Value> =
            serde_json::from_str(text).map_err(|e| format!("not a JSON list: {e}"))?;
        return Ok(items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            })
            .collect());
    }
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Ids sent for a many-to-many field: a JSON array, or a comma-separated list.
fn parse_ids(text: &str) -> Result<Vec<String>, String> {
    let mut ids = if text.starts_with('[') {
        list_items(text)?
    } else {
        text.split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect()
    };
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// A JSON value as the text used for ids in URLs and forms.
fn text_of(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn number<T>(raw: Option<&str>) -> Result<Option<T>, String>
where
    T: FromStr,
    T::Err: Display,
{
    raw.map(|text| text.trim().parse::<T>().map_err(|e| e.to_string()))
        .transpose()
}

fn naive_datetime(text: &str) -> Result<NaiveDateTime, String> {
    // `datetime-local` inputs omit seconds; databases usually print a space separator.
    let text = text.trim().replace(' ', "T");
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(&text, format).ok())
        // A bare date means its start, as in `?created_at__gte=2026-01-01`.
        .or_else(|| {
            NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                .ok()
                .map(|day| day.and_time(NaiveTime::MIN))
        })
        .ok_or_else(|| format!("`{text}` is not a date and time"))
}

/// A stored date-time as ISO 8601, with a zone offset only if the column has one; `None` if
/// the text is not a date-time this crate understands, in which case it is left as it is.
fn iso_datetime(ty: &ColumnType, text: &str) -> Option<String> {
    if matches!(ty, ColumnType::TimestampWithTimeZone) {
        let text = text.trim().replacen(' ', "T", 1);
        return DateTime::<FixedOffset>::parse_from_rfc3339(&text)
            .ok()
            .map(|d| d.to_rfc3339());
    }
    naive_datetime(text)
        .ok()
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
}

/// Converts form text to the SeaORM value for `ty`; `None` produces a typed NULL.
fn parse_value(ty: &ColumnType, raw: Option<&str>) -> Result<Value, String> {
    Ok(match ty {
        ColumnType::Char(_) | ColumnType::String(_) | ColumnType::Text => {
            Value::String(raw.map(str::to_owned))
        }
        ColumnType::TinyInteger => Value::TinyInt(number(raw)?),
        ColumnType::SmallInteger => Value::SmallInt(number(raw)?),
        ColumnType::Integer => Value::Int(number(raw)?),
        ColumnType::BigInteger => Value::BigInt(number(raw)?),
        ColumnType::TinyUnsigned => Value::TinyUnsigned(number(raw)?),
        ColumnType::SmallUnsigned => Value::SmallUnsigned(number(raw)?),
        ColumnType::Unsigned => Value::Unsigned(number(raw)?),
        ColumnType::BigUnsigned => Value::BigUnsigned(number(raw)?),
        ColumnType::Float => Value::Float(number(raw)?),
        ColumnType::Double => Value::Double(number(raw)?),
        ColumnType::Boolean => Value::Bool(raw.map(boolean).transpose()?),
        ColumnType::DateTime | ColumnType::Timestamp => {
            Value::ChronoDateTime(raw.map(naive_datetime).transpose()?)
        }
        ColumnType::TimestampWithTimeZone => Value::ChronoDateTimeWithTimeZone(
            raw.map(|text| {
                DateTime::<FixedOffset>::parse_from_rfc3339(text.trim())
                    .or_else(|_| naive_datetime(text).map(|n| n.and_utc().fixed_offset()))
            })
            .transpose()?,
        ),
        ColumnType::Date => Value::ChronoDate(
            raw.map(|t| NaiveDate::parse_from_str(t.trim(), "%Y-%m-%d").map_err(|e| e.to_string()))
                .transpose()?,
        ),
        ColumnType::Time => Value::ChronoTime(
            raw.map(|t| {
                let t = t.trim();
                NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
                    .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M"))
                    .map_err(|e| e.to_string())
            })
            .transpose()?,
        ),
        ColumnType::Json | ColumnType::JsonBinary => Value::Json(
            raw.map(|t| {
                serde_json::from_str(t)
                    .map(Box::new)
                    .map_err(|e| e.to_string())
            })
            .transpose()?,
        ),
        ColumnType::Uuid => Value::Uuid(
            raw.map(|t| {
                Uuid::parse_str(t.trim()).map_err(|_| format!("`{}` is not a UUID", t.trim()))
            })
            .transpose()?,
        ),
        ColumnType::Decimal(_) | ColumnType::Money(_) => Value::Decimal(
            raw.map(|t| {
                Decimal::from_str(t.trim()).map_err(|_| format!("`{}` is not a number", t.trim()))
            })
            .transpose()?,
        ),
        // The model's own enum type turns the text into its value, refusing unknown ones.
        ColumnType::Enum { .. } => Value::String(raw.map(str::to_owned)),
        ColumnType::Array(inner) => {
            let element =
                array_type(inner).ok_or_else(|| format!("lists of {inner:?} are not editable"))?;
            let items = raw
                .map(|text| {
                    list_items(text)?
                        .iter()
                        .map(|item| parse_value(inner, Some(item)))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?;
            Value::Array(element, items.map(Box::new))
        }
        other => return Err(format!("column type {other:?} is not editable")),
    })
}

// ---- many-to-many --------------------------------------------------------------------------

/// The table that links two models, behind a many-to-many field.
#[async_trait]
pub(crate) trait LinkTable: Send + Sync {
    /// What the link points to: the other model and its key column.
    fn target(&self) -> Option<&Reference>;
    /// The link table, for migrations.
    fn table(&self) -> Result<EntityTable, String>;
    /// Adds the link table to a schema sync.
    fn register(&self, builder: SchemaBuilder) -> SchemaBuilder;
    /// `(owner id, target id)` for every link of these owners.
    async fn pairs(
        &self,
        db: &DatabaseConnection,
        owners: &[String],
    ) -> Result<Vec<(String, serde_json::Value)>, AdminError>;
    /// Makes `targets` the only links of `owner`.
    async fn replace(
        &self,
        txn: &DatabaseTransaction,
        owner: &str,
        targets: &[String],
    ) -> Result<(), AdminError>;
    /// Removes every link of `owner`.
    async fn clear(&self, txn: &DatabaseTransaction, owner: &str) -> Result<(), AdminError>;
    /// Those of `targets` that do not exist.
    async fn missing(
        &self,
        txn: &DatabaseTransaction,
        targets: &[String],
    ) -> Result<Vec<String>, AdminError>;
    /// A query for the owners linked to `target`.
    fn owners_linked_to(&self, target: &str) -> Result<SelectStatement, AdminError>;
}

struct Junction<J: EntityTrait> {
    from: J::Column,
    to: J::Column,
    from_type: ColumnType,
    to_type: ColumnType,
    target: Option<Reference>,
    entity: PhantomData<fn() -> J>,
}

impl<J: EntityTrait> Junction<J> {
    fn values(&self, ty: &ColumnType, ids: &[String]) -> Vec<Value> {
        // An id that does not parse names nothing.
        ids.iter()
            .filter_map(|id| parse_value(ty, Some(id)).ok())
            .collect()
    }

    fn owner(&self, owner: &str) -> Result<Value, AdminError> {
        parse_value(&self.from_type, Some(owner))
            .map_err(|_| AdminError::NotFound(format!("`{owner}`")))
    }
}

#[async_trait]
impl<J> LinkTable for Junction<J>
where
    J: EntityTrait + Default,
    J::ActiveModel: ActiveModelBehavior + Send,
{
    fn target(&self) -> Option<&Reference> {
        self.target.as_ref()
    }

    fn table(&self) -> Result<EntityTable, String> {
        crate::migrations::table_of::<J>()
    }

    fn register(&self, builder: SchemaBuilder) -> SchemaBuilder {
        builder.register(J::default())
    }

    async fn pairs(
        &self,
        db: &DatabaseConnection,
        owners: &[String],
    ) -> Result<Vec<(String, serde_json::Value)>, AdminError> {
        let values = self.values(&self.from_type, owners);
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let rows = J::find()
            .filter(self.from.is_in(values))
            .order_by_asc(self.to)
            .into_json()
            .all(db)
            .await?;
        let (from, to) = (self.from.as_str(), self.to.as_str());
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let owner = text_of(row.get(from));
                let target = row.get(to)?.clone();
                Some((owner, target))
            })
            .collect())
    }

    async fn replace(
        &self,
        txn: &DatabaseTransaction,
        owner: &str,
        targets: &[String],
    ) -> Result<(), AdminError> {
        self.clear(txn, owner).await?;
        let owner = self.owner(owner)?;
        let mut models = Vec::new();
        for target in self.values(&self.to_type, targets) {
            let mut model = <J::ActiveModel as ActiveModelTrait>::default();
            model.try_set(self.from, owner.clone())?;
            model.try_set(self.to, target)?;
            models.push(model);
        }
        if !models.is_empty() {
            J::insert_many(models).exec(txn).await?;
        }
        Ok(())
    }

    async fn clear(&self, txn: &DatabaseTransaction, owner: &str) -> Result<(), AdminError> {
        J::delete_many()
            .filter(self.from.eq(self.owner(owner)?))
            .exec(txn)
            .await?;
        Ok(())
    }

    async fn missing(
        &self,
        txn: &DatabaseTransaction,
        targets: &[String],
    ) -> Result<Vec<String>, AdminError> {
        let Some(target) = &self.target else {
            return Ok(Vec::new());
        };
        use sea_orm::sea_query::ExprTrait;
        let mut missing = Vec::new();
        for id in targets {
            let Ok(value) = parse_value(&self.to_type, Some(id)) else {
                missing.push(id.clone());
                continue;
            };
            let query = Query::select()
                .expr(Expr::val(1))
                .from(Alias::new(&target.model))
                .and_where(Expr::col(Alias::new(&target.column)).eq(value))
                .limit(1)
                .to_owned();
            let backend = txn.get_database_backend();
            if txn.query_one_raw(backend.build(&query)).await?.is_none() {
                missing.push(id.clone());
            }
        }
        Ok(missing)
    }

    fn owners_linked_to(&self, target: &str) -> Result<SelectStatement, AdminError> {
        use sea_orm::sea_query::ExprTrait;
        let value =
            parse_value(&self.to_type, Some(target)).map_err(|message| AdminError::BadInput {
                field: self.to.as_str().to_owned(),
                message,
            })?;
        Ok(Query::select()
            .column(self.from)
            .from(J::default())
            .and_where(Expr::col(self.to).eq(value))
            .to_owned())
    }
}

impl ModelOptions {
    /// A many-to-many field, kept in the link table `J`: `from` is its column pointing to
    /// this model, `to` its column pointing to the other one (each with a `belongs_to`
    /// relation in `J`). The admin offers checkboxes of the other model's rows, the list can
    /// show and filter by it, and the API reads and writes it as a list of ids:
    ///
    /// ```ignore
    /// ModelOptions::new().many_to_many::<post_tag::Entity>("tags", post_tag::Column::PostId, post_tag::Column::TagId)
    /// ```
    ///
    /// The link table's own table is created by migrations with this model's.
    #[must_use]
    pub fn many_to_many<J>(
        mut self,
        name: impl Into<String>,
        from: J::Column,
        to: J::Column,
    ) -> Self
    where
        J: EntityTrait + Default,
        J::ActiveModel: ActiveModelBehavior + Send,
    {
        let table = J::default().table_name().to_owned();
        let target = references::<J>(&table)
            .into_iter()
            .find(|(column, _)| column == to.as_str())
            .map(|(_, reference)| reference);
        let name = name.into();
        let link = Junction::<J> {
            from,
            to,
            from_type: from.def().get_column_type().clone(),
            to_type: to.def().get_column_type().clone(),
            target,
            entity: PhantomData,
        };
        self.many_to_many.push(ManyToMany {
            label: humanize(&name),
            name,
            link: Arc::new(link),
        });
        self
    }
}
