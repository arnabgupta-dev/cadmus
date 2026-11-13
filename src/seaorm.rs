//! [`Resource`] for any SeaORM entity, built from the entity's own column metadata.

use std::fmt::Display;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sea_orm::sea_query::ColumnType;
use sea_orm::{
    ActiveModelBehavior, ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, DbErr,
    EntityTrait, IdenStatic, IntoActiveModel, Iterable, ModelTrait, PaginatorTrait,
    PrimaryKeyToColumn, PrimaryKeyTrait, QueryFilter, QueryOrder, Value,
};

use crate::error::AdminError;
use crate::resource::{
    Field, FieldKind, Input, ListQuery, ModelOptions, Page, Resource, Row, WriteMode, humanize,
};

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
            fields.push(Field {
                name: name.to_owned(),
                label: humanize(name),
                kind,
                nullable: def.is_null(),
                primary_key,
                auto: primary_key && auto,
                editable: kind != FieldKind::Other,
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
        self.options = options;
        self
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
            let provided = match input.get(&field.name) {
                None if mode == WriteMode::Patch => continue,
                // Let the database apply its declared default on insert.
                None if creating && (field.nullable || self.has_default[i]) => continue,
                None | Some(None) => None,
                // An empty box in a form means "no value" for everything except text.
                Some(Some(text)) if text.is_empty() && !is_text => None,
                Some(Some(text)) => Some(text.as_str()),
            };
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

    async fn list(&self, query: &ListQuery) -> Result<Page, AdminError> {
        let mut select = E::find();
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
                    any = any.add(self.columns[i].contains(term));
                    searchable += 1;
                }
            }
            if searchable > 0 {
                select = select.filter(any);
            }
        }
        if let Some(i) = self.pk {
            select = select.order_by_asc(self.columns[i]);
        }
        let paginator = select.into_json().paginate(&self.db, query.per_page.max(1));
        let total = paginator.num_items().await?;
        let rows = paginator
            .fetch_page(query.page)
            .await?
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            })
            .collect();
        Ok(Page { rows, total })
    }

    async fn get(&self, id: &str) -> Result<Option<Row>, AdminError> {
        let (column, value) = match self.pk_value(id) {
            Ok(pair) => pair,
            Err(AdminError::NotFound(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        let row = E::find()
            .filter(column.eq(value))
            .into_json()
            .one(&self.db)
            .await?;
        Ok(match row {
            Some(serde_json::Value::Object(map)) => Some(map),
            _ => None,
        })
    }

    async fn create(&self, input: &Input) -> Result<Option<Row>, AdminError> {
        let mut model = <E::ActiveModel as ActiveModelTrait>::default();
        self.apply_input(&mut model, input, true, WriteMode::Replace)?;
        let saved = model.insert(&self.db).await?;
        let Some(i) = self.pk else { return Ok(None) };
        match id_text(&saved.get(self.columns[i])) {
            Some(id) => self.get(&id).await,
            None => Ok(None),
        }
    }

    async fn update(&self, id: &str, input: &Input, mode: WriteMode) -> Result<(), AdminError> {
        let (column, value) = self.pk_value(id)?;
        let mut model = <E::ActiveModel as ActiveModelTrait>::default();
        model.try_set(column, value)?;
        self.apply_input(&mut model, input, false, mode)?;
        match model.update(&self.db).await {
            Ok(_) => Ok(()),
            Err(DbErr::RecordNotUpdated) => {
                Err(AdminError::NotFound(format!("{} `{id}`", self.slug)))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn delete(&self, id: &str) -> Result<(), AdminError> {
        let (column, value) = self.pk_value(id)?;
        let result = E::delete_many()
            .filter(column.eq(value))
            .exec(&self.db)
            .await?;
        if result.rows_affected == 0 {
            return Err(AdminError::NotFound(format!("{} `{id}`", self.slug)));
        }
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
        _ => FieldKind::Other,
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
        .ok_or_else(|| format!("`{text}` is not a date and time"))
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
        other => return Err(format!("column type {other:?} is not editable")),
    })
}
