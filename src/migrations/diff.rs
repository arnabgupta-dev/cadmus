//! Works out the operations that turn the migrations' tables into the models' tables.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};

use super::{Column, EntityTable, Operation, State, Table, Value};

/// What `makemigrations` cannot tell from the models alone.
pub(crate) trait Questions {
    /// Whether table `from` was renamed to `to` (rather than dropped and another created).
    fn renamed_table(&mut self, from: &str, to: &str) -> bool;
    /// Whether `table.from` was renamed to `to`.
    fn renamed_column(&mut self, table: &str, from: &str, to: &str) -> bool;
    /// The value rows already in `table` get for a new required column without a default.
    fn fill(&mut self, table: &str, column: &Column) -> Result<Value, String>;
}

/// Asks in the terminal when there is one; otherwise assumes nothing was renamed and refuses
/// to guess values.
pub(crate) struct Answers {
    interactive: bool,
}

impl Answers {
    pub(crate) fn new(no_input: bool) -> Self {
        Self {
            interactive: !no_input && std::io::stdin().is_terminal(),
        }
    }

    fn ask(&self, question: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        print!("{question}");
        std::io::stdout().flush().ok()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        Some(line.trim().to_owned())
    }

    fn yes(&self, question: &str) -> bool {
        self.ask(&format!("{question} [y/N] "))
            .is_some_and(|a| matches!(a.to_lowercase().as_str(), "y" | "yes"))
    }
}

impl Questions for Answers {
    fn renamed_table(&mut self, from: &str, to: &str) -> bool {
        self.yes(&format!("Was the table `{from}` renamed to `{to}`?"))
    }

    fn renamed_column(&mut self, table: &str, from: &str, to: &str) -> bool {
        self.yes(&format!("Was `{table}.{from}` renamed to `{table}.{to}`?"))
    }

    fn fill(&mut self, table: &str, column: &Column) -> Result<Value, String> {
        let problem = format!(
            "`{table}.{}` is required and has no default, so rows already in `{table}` need a \
             value for it",
            column.name
        );
        if !self.interactive {
            return Err(format!(
                "{problem}. Give the field a default (`#[sea_orm(default_value = ...)]`), make \
                 it an `Option`, or run makemigrations in a terminal to type a value"
            ));
        }
        let typed = self
            .ask(&format!(
                "{problem}.\nType the value (text, a number, true or false), or press Enter to \
                 stop: "
            ))
            .unwrap_or_default();
        if typed.is_empty() {
            return Err("stopped: no value was given".into());
        }
        Ok(parse_value(&typed))
    }
}

/// Answers that only find out whether anything changed: nothing renamed, and a stand-in
/// value for new required columns.
pub(crate) struct Detect;

impl Questions for Detect {
    fn renamed_table(&mut self, _: &str, _: &str) -> bool {
        false
    }

    fn renamed_column(&mut self, _: &str, _: &str, _: &str) -> bool {
        false
    }

    fn fill(&mut self, _: &str, _: &Column) -> Result<Value, String> {
        Ok(Value::sql("NULL"))
    }
}

/// A value typed in the terminal: true/false, a number, or text (quotes optional).
fn parse_value(typed: &str) -> Value {
    match typed {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = typed.parse::<i64>() {
        return Value::Int(n);
    }
    if let Ok(n) = typed.parse::<f64>() {
        return Value::Float(n);
    }
    let unquoted = typed
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| typed.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(typed);
    Value::Text(unquoted.to_owned())
}

fn same_columns(a: &Table, b: &Table) -> bool {
    a.columns.len() == b.columns.len()
        && a.columns.iter().all(|column| {
            b.find(&column.name)
                .is_some_and(|other| other.same_definition(column))
        })
}

/// The operations that turn `from` into the `target` tables, in a safe order: renames, then
/// dropped keys and indexes, new tables, new, changed and dropped columns, dropped tables,
/// and finally new indexes and keys.
pub(crate) fn diff(
    from: &State,
    target: &[EntityTable],
    questions: &mut dyn Questions,
) -> Result<Vec<Operation>, String> {
    let wanted: BTreeMap<&str, &EntityTable> =
        target.iter().map(|t| (t.table.name.as_str(), t)).collect();
    let mut state = from.clone();
    let mut operations = Vec::new();
    let mut emit = |state: &mut State, operation: Operation| -> Result<(), String> {
        state.apply(&operation)?;
        operations.push(operation);
        Ok(())
    };

    // Renamed tables: a dropped and a new table with the same columns.
    let mut removed: Vec<String> = state
        .tables
        .keys()
        .filter(|name| !wanted.contains_key(name.as_str()))
        .cloned()
        .collect();
    for (name, entity) in &wanted {
        if state.tables.contains_key(*name) {
            continue;
        }
        let found = removed.iter().position(|old| {
            same_columns(&state.tables[old], &entity.table) && questions.renamed_table(old, name)
        });
        if let Some(i) = found {
            let old = removed.remove(i);
            emit(
                &mut state,
                Operation::RenameTable {
                    from: old,
                    to: (*name).to_owned(),
                },
            )?;
        }
    }

    // Renamed columns: marked `renamed_from` on the model, or confirmed.
    for (name, entity) in &wanted {
        let Some(current) = state.tables.get(*name).cloned() else {
            continue;
        };
        let mut gone: Vec<&Column> = current
            .columns
            .iter()
            .filter(|c| entity.table.find(&c.name).is_none())
            .collect();
        for column in &entity.table.columns {
            if current.find(&column.name).is_some() {
                continue;
            }
            let hinted = entity
                .renamed_from
                .get(&column.name)
                .and_then(|old| gone.iter().position(|c| &c.name == old));
            let found = hinted.or_else(|| {
                gone.iter().position(|old| {
                    old.same_definition(column)
                        && questions.renamed_column(name, &old.name, &column.name)
                })
            });
            if let Some(i) = found {
                let old = gone.remove(i);
                emit(
                    &mut state,
                    Operation::RenameColumn {
                        table: (*name).to_owned(),
                        from: old.name.clone(),
                        to: column.name.clone(),
                    },
                )?;
            }
        }
    }

    let mut drop_keys = Vec::new();
    let mut drop_indexes = Vec::new();
    let mut add_columns = Vec::new();
    let mut alter_columns = Vec::new();
    let mut drop_columns = Vec::new();
    let mut add_indexes = Vec::new();
    let mut add_keys = Vec::new();

    for (name, entity) in &wanted {
        let Some(current) = state.tables.get(*name) else {
            continue;
        };
        let want = &entity.table;
        let table = (*name).to_owned();
        if current.primary_key != want.primary_key {
            return Err(format!(
                "the primary key of `{name}` changed; migrations cannot change a primary key \
                 over several columns, so write that migration by hand with `.sql(...)`"
            ));
        }
        for key in &current.foreign_keys {
            if !want.foreign_keys.iter().any(|k| k.same_definition(key)) {
                drop_keys.push(Operation::DropForeignKey {
                    table: table.clone(),
                    key: key.clone(),
                });
            }
        }
        for key in &want.foreign_keys {
            if !current.foreign_keys.iter().any(|k| k.same_definition(key)) {
                add_keys.push(Operation::AddForeignKey {
                    table: table.clone(),
                    key: key.clone(),
                });
            }
        }
        for index in &current.indexes {
            if !want.indexes.contains(index) {
                drop_indexes.push(Operation::DropIndex {
                    table: table.clone(),
                    index: index.clone(),
                });
            }
        }
        for index in &want.indexes {
            if !current.indexes.contains(index) {
                add_indexes.push(Operation::AddIndex {
                    table: table.clone(),
                    index: index.clone(),
                });
            }
        }
        for column in &want.columns {
            match current.find(&column.name) {
                Some(existing) if existing == column => {}
                Some(existing) => alter_columns.push(Operation::AlterColumn {
                    table: table.clone(),
                    from: existing.clone(),
                    to: column.clone(),
                }),
                None => {
                    let fill = if column.needs_fill() {
                        Some(questions.fill(name, column)?)
                    } else {
                        None
                    };
                    add_columns.push(Operation::AddColumn {
                        table: table.clone(),
                        column: column.clone(),
                        fill,
                    });
                }
            }
        }
        for column in &current.columns {
            if want.find(&column.name).is_none() {
                drop_columns.push(Operation::DropColumn {
                    table: table.clone(),
                    column: column.clone(),
                });
            }
        }
    }

    // New tables, each after the new tables it refers to. In a cycle, one table is created
    // without its keys to the others, which are added at the end.
    let mut pending: Vec<Table> = wanted
        .iter()
        .filter(|(name, _)| !state.tables.contains_key(**name))
        .map(|(_, entity)| entity.table.clone())
        .collect();
    let mut creates = Vec::new();
    let mut created: BTreeSet<String> = BTreeSet::new();
    while !pending.is_empty() {
        let waiting: BTreeSet<String> = pending.iter().map(|t| t.name.clone()).collect();
        let ready = pending.iter().position(|t| {
            t.foreign_keys
                .iter()
                .all(|k| k.to_table == t.name || !waiting.contains(&k.to_table))
        });
        let mut table = pending.remove(ready.unwrap_or(0));
        if ready.is_none() {
            let (later, now): (Vec<_>, Vec<_>) = table
                .foreign_keys
                .into_iter()
                .partition(|k| k.to_table != table.name && waiting.contains(&k.to_table));
            table.foreign_keys = now;
            for key in later {
                add_keys.push(Operation::AddForeignKey {
                    table: table.name.clone(),
                    key,
                });
            }
        }
        created.insert(table.name.clone());
        creates.push(Operation::CreateTable(table));
    }

    // Dropped tables, each before the dropped tables it refers to. In a cycle, keys are
    // dropped first.
    let mut doomed: Vec<String> = removed;
    let mut drop_order = Vec::new();
    while !doomed.is_empty() {
        let referenced = |name: &str, doomed: &[String]| {
            doomed.iter().any(|other| {
                other != name
                    && state.tables[other]
                        .foreign_keys
                        .iter()
                        .any(|k| k.to_table == name)
            })
        };
        let ready = doomed.iter().position(|name| !referenced(name, &doomed));
        let name = doomed.remove(ready.unwrap_or(0));
        if ready.is_none() {
            for other in &doomed {
                for key in &state.tables[other].foreign_keys {
                    if key.to_table == name {
                        drop_keys.push(Operation::DropForeignKey {
                            table: other.clone(),
                            key: key.clone(),
                        });
                    }
                }
            }
        }
        drop_order.push(name);
    }

    for operation in drop_keys
        .into_iter()
        .chain(drop_indexes)
        .chain(creates)
        .chain(add_columns)
        .chain(alter_columns)
        .chain(drop_columns)
    {
        emit(&mut state, operation)?;
    }
    for name in drop_order {
        let table = state.tables[&name].clone();
        emit(&mut state, Operation::DropTable(table))?;
    }
    for operation in add_indexes.into_iter().chain(add_keys) {
        emit(&mut state, operation)?;
    }
    Ok(operations)
}

/// One line per operation, for the terminal.
pub(crate) fn describe(operations: &[Operation]) -> Vec<String> {
    operations
        .iter()
        .map(|operation| match operation {
            Operation::CreateTable(t) => format!("+ table {}", t.name),
            Operation::DropTable(t) => format!("- table {} (its rows will be deleted)", t.name),
            Operation::RenameTable { from, to } => format!("> table {from} renamed to {to}"),
            Operation::AddColumn {
                table,
                column,
                fill,
            } => match fill {
                Some(value) => format!(
                    "+ column {table}.{} (existing rows get {})",
                    column.name,
                    super::codegen::value(value)
                ),
                None => format!("+ column {table}.{}", column.name),
            },
            Operation::DropColumn { table, column } => {
                format!(
                    "- column {table}.{} (its data will be deleted)",
                    column.name
                )
            }
            Operation::RenameColumn { table, from, to } => {
                format!("> column {table}.{from} renamed to {to}")
            }
            Operation::AlterColumn { table, to, .. } => format!("~ column {table}.{}", to.name),
            Operation::AddIndex { index, .. } => format!("+ index {}", index.name),
            Operation::DropIndex { index, .. } => format!("- index {}", index.name),
            Operation::AddForeignKey { key, .. } => format!("+ foreign key {}", key.name),
            Operation::DropForeignKey { key, .. } => format!("- foreign key {}", key.name),
            Operation::Sql { .. } => "! SQL".to_owned(),
            Operation::Run { .. } => "! Rust code".to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{ForeignKey, Type};

    struct Yes;

    impl Questions for Yes {
        fn renamed_table(&mut self, _: &str, _: &str) -> bool {
            true
        }
        fn renamed_column(&mut self, _: &str, _: &str, _: &str) -> bool {
            true
        }
        fn fill(&mut self, _: &str, _: &Column) -> Result<Value, String> {
            Ok(Value::Text("x".into()))
        }
    }

    fn entity(table: Table) -> EntityTable {
        EntityTable {
            table,
            renamed_from: BTreeMap::new(),
        }
    }

    fn id() -> Column {
        Column::new("id", Type::Integer)
            .primary_key()
            .auto_increment()
    }

    #[test]
    fn new_tables_come_after_the_tables_they_refer_to_and_cycles_are_split() {
        let a = Table::new("a")
            .column(id())
            .column(Column::new("b_id", Type::Integer).null())
            .foreign_key(ForeignKey::new("fk-a-b", ["b_id"], "b", ["id"]));
        let b = Table::new("b")
            .column(id())
            .column(Column::new("a_id", Type::Integer).null())
            .foreign_key(ForeignKey::new("fk-b-a", ["a_id"], "a", ["id"]));
        let c = Table::new("c")
            .column(id())
            .column(Column::new("d_id", Type::Integer))
            .foreign_key(ForeignKey::new("fk-c-d", ["d_id"], "d", ["id"]));
        let d = Table::new("d").column(id());
        let ops = diff(
            &State::default(),
            &[entity(a), entity(b), entity(c), entity(d)],
            &mut Yes,
        )
        .unwrap();
        let order: Vec<String> = ops
            .iter()
            .map(|op| match op {
                Operation::CreateTable(t) => format!("create {}", t.name),
                Operation::AddForeignKey { key, .. } => format!("key {}", key.name),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            order,
            ["create d", "create c", "create a", "create b", "key fk-a-b"]
        );
    }

    #[test]
    fn non_interactive_answers_assume_no_rename_and_refuse_to_fill() {
        let before = State::after(&[crate::migrations::Migration::new("0001").create_table(
            Table::new("t")
                .column(id())
                .column(Column::new("old", Type::Text)),
        )])
        .unwrap();
        let after = Table::new("t")
            .column(id())
            .column(Column::new("new", Type::Text));
        let error = diff(&before, &[entity(after.clone())], &mut Answers::new(true)).unwrap_err();
        assert!(error.contains("`t.new` is required"), "{error}");
        let ops = diff(&before, &[entity(after)], &mut Yes).unwrap();
        assert!(
            matches!(&ops[..], [Operation::RenameColumn { from, to, .. }] if from == "old" && to == "new")
        );
    }

    #[test]
    fn typed_values() {
        assert_eq!(parse_value("true"), Value::Bool(true));
        assert_eq!(parse_value("12"), Value::Int(12));
        assert_eq!(parse_value("1.5"), Value::Float(1.5));
        assert_eq!(parse_value("'draft'"), Value::Text("draft".into()));
        assert_eq!(parse_value("draft"), Value::Text("draft".into()));
    }
}
