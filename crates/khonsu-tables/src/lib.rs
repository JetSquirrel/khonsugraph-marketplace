//! Relational tables as a graph: a mapping says which tables are nodes and
//! which are relationships, and [`run`] reads them through a [`Driver`] and
//! reports the records.
//!
//! A mapping is either written out (`nodes` and `edges` in a source's
//! configuration) or [inferred](infer) from primary and foreign keys: a table
//! with a primary key is a node, a foreign key is a relationship, and a table
//! that only joins two others is a relationship between them.
//!
//! This crate knows no database. A plugin supplies the [`Driver`]: how to
//! list tables, quote a name and run a `SELECT`.

use khonsu_plugin_protocol::{EdgeRecord, NodeRecord};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A table as the source's catalog describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    /// Empty when the table has no primary key.
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// One row: column name to value. A null is `Value::Null`.
pub type Row = Map<String, Value>;

/// What a plugin supplies about its database.
pub trait Driver {
    /// The tables that can be read, with their keys.
    fn catalog(&mut self) -> Result<Vec<Table>, String>;
    /// `name` as an identifier: `"name"`, or `` `name` ``.
    fn quote(&self, name: &str) -> String;
    /// `table` as it is written after `FROM`, with its schema where there is one.
    fn table_ref(&self, table: &str) -> String {
        self.quote(table)
    }
    /// `expr` as text, for a cursor that reads back the same on any column type.
    fn as_text(&self, expr: &str) -> String;
    /// Runs one `SELECT`, handing over each row as it arrives. An error from
    /// `on_row` ends the query and is returned.
    fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String>;
}

/// One column, or several that make a key together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Columns {
    One(String),
    Many(Vec<String>),
}

impl Columns {
    pub fn list(&self) -> Vec<String> {
        match self {
            Columns::One(column) => vec![column.clone()],
            Columns::Many(columns) => columns.clone(),
        }
    }

    fn from_list(mut columns: Vec<String>) -> Self {
        if columns.len() == 1 {
            Columns::One(columns.remove(0))
        } else {
            Columns::Many(columns)
        }
    }
}

/// A table whose rows are nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeMap {
    /// The table to read. Either this or `query`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// A `SELECT` to read instead of a table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub label: String,
    /// The column, or columns, that identify a row.
    pub id: Columns,
    /// The columns kept as properties; all of them when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// A condition on the rows read, as SQL.
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// A column that only grows as rows change, for incremental syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// One end of a relationship: the node it is, by label and key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct End {
    /// The label of the node table this end points at.
    pub label: String,
    /// The column, or columns, of this table holding that node's key.
    #[serde(alias = "column")]
    pub columns: Columns,
}

/// A table whose rows are relationships.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeMap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(rename = "type")]
    pub rel_type: String,
    pub from: End,
    pub to: End,
    /// The column, or columns, that identify a row, when two nodes can be
    /// joined more than once. Without it, one relationship per pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Columns>,
    /// The columns kept as properties; all but the ends when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    #[serde(default)]
    pub nodes: Vec<NodeMap>,
    #[serde(default)]
    pub edges: Vec<EdgeMap>,
}

impl Mapping {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    pub fn labels(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.label.clone()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    pub fn relationship_types(&self) -> Vec<String> {
        self.edges.iter().map(|e| e.rel_type.clone()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Checks what can be checked without the database.
    pub fn validate(&self) -> Result<(), String> {
        let source = |table: &Option<String>, query: &Option<String>, what: &str| match (table, query) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => Err(format!("{what}: give table or query, not both")),
            (None, None) => Err(format!("{what}: give a table or a query")),
        };
        let labels: BTreeSet<&str> = self.nodes.iter().map(|n| n.label.as_str()).collect();
        for node in &self.nodes {
            let what = format!("node {}", node.label);
            source(&node.table, &node.query, &what)?;
            if node.label.trim().is_empty() {
                return Err("a node table needs a label".into());
            }
            if node.id.list().is_empty() {
                return Err(format!("{what}: id names no column"));
            }
        }
        for edge in &self.edges {
            let what = format!("edge {}", edge.rel_type);
            source(&edge.table, &edge.query, &what)?;
            if edge.rel_type.trim().is_empty() {
                return Err("an edge table needs a type".into());
            }
            for (name, end) in [("from", &edge.from), ("to", &edge.to)] {
                if end.columns.list().is_empty() {
                    return Err(format!("{what}: {name} names no column"));
                }
                if !labels.contains(end.label.as_str()) {
                    let known: Vec<&str> = labels.iter().copied().collect();
                    return Err(format!(
                        "{what}: {name}.label {:?} is not the label of a node table (labels: {})",
                        end.label,
                        known.join(", ")
                    ));
                }
                let key = self.nodes.iter().find(|n| n.label == end.label).map(|n| n.id.list().len()).unwrap_or(0);
                if key != end.columns.list().len() {
                    return Err(format!(
                        "{what}: {name} has {} column(s), and {} is identified by {key}",
                        end.columns.list().len(),
                        end.label
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The `nodes` and `edges` of a source's configuration: JSON, or JSON as
/// text. `None` when neither is given, which asks for the mapping to be inferred.
pub fn mapping_from_config(config: &Map<String, Value>) -> Result<Option<Mapping>, String> {
    fn list<T: serde::de::DeserializeOwned>(config: &Map<String, Value>, key: &str) -> Result<Vec<T>, String> {
        match config.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(text)) if text.trim().is_empty() => Ok(Vec::new()),
            Some(Value::String(text)) => serde_json::from_str(text).map_err(|e| format!("{key}: {e}")),
            Some(value) => serde_json::from_value(value.clone()).map_err(|e| format!("{key}: {e}")),
        }
    }
    let mapping = Mapping {
        nodes: list(config, "nodes")?,
        edges: list(config, "edges")?,
    };
    if mapping.is_empty() {
        return Ok(None);
    }
    mapping.validate()?;
    Ok(Some(mapping))
}

/// `order_items` -> `OrderItems`.
pub fn label_for(table: &str) -> String {
    let mut label = String::new();
    for word in table.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            label.extend(first.to_uppercase());
            label.push_str(chars.as_str());
        }
    }
    if label.is_empty() || label.starts_with(|c: char| c.is_ascii_digit()) {
        label.insert(0, 'T');
    }
    label
}

/// `orderItems`, `order-items` -> `ORDER_ITEMS`.
pub fn type_for(name: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && previous_lower {
                out.push('_');
            }
            out.extend(c.to_uppercase());
            previous_lower = c.is_lowercase() || c.is_ascii_digit();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            previous_lower = false;
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "RELATED_TO".into()
    } else {
        out
    }
}

/// The type of the relationship a foreign key makes: its column without the
/// `_id`, or the table it points at when that leaves nothing to go by.
fn type_for_key(key: &ForeignKey) -> String {
    if let [column] = key.columns.as_slice() {
        let lower = column.to_lowercase();
        for suffix in ["_id", "_fk", "_key", "id"] {
            if lower.len() > suffix.len() && lower.ends_with(suffix) {
                let stem = &column[..column.len() - suffix.len()];
                if stem.chars().any(char::is_alphanumeric) {
                    return type_for(stem);
                }
            }
        }
    }
    type_for(&key.ref_table)
}

/// A mapping from keys alone, and what was left out of it and why.
///
/// - A table with a primary key is a node table, labelled after the table.
/// - Each of its foreign keys to another node table's primary key is a
///   relationship from the row to the row it points at.
/// - A table with exactly two such foreign keys and no key of its own beyond
///   them only joins two tables: it is a relationship between them, and its
///   other columns are the relationship's properties.
///
/// `incremental_column`, where a table has it, is what incremental syncs go by.
pub fn infer(tables: &[Table], incremental_column: Option<&str>) -> (Mapping, Vec<String>) {
    let mut notes = Vec::new();
    let same = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.eq_ignore_ascii_case(b));
    let by_name = |name: &str| tables.iter().find(|t| t.name == name);
    // A foreign key that lands on the whole primary key of its table.
    let points_at_key = |key: &ForeignKey| by_name(&key.ref_table).is_some_and(|t| !t.primary_key.is_empty() && same(&t.primary_key, &key.ref_columns));
    let is_join = |table: &Table| {
        let [a, b] = table.foreign_keys.as_slice() else {
            return false;
        };
        if !points_at_key(a) || !points_at_key(b) {
            return false;
        }
        let ends: BTreeSet<String> = a.columns.iter().chain(&b.columns).map(|c| c.to_lowercase()).collect();
        let key: BTreeSet<String> = table.primary_key.iter().map(|c| c.to_lowercase()).collect();
        key.is_empty() || key == ends
    };
    let updated = |table: &Table| incremental_column.and_then(|wanted| table.columns.iter().find(|c| c.eq_ignore_ascii_case(wanted)).cloned());

    let mut mapping = Mapping::default();
    let node_tables: Vec<&Table> = tables.iter().filter(|t| !t.primary_key.is_empty() && !is_join(t)).collect();
    let is_node = |name: &str| node_tables.iter().any(|t| t.name == name);
    let mut used_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut unique = |rel_type: String, from: &str| {
        let count = used_types.entry(rel_type.clone()).or_default();
        *count += 1;
        // Two foreign keys that would share a type between other tables stay
        // apart: the second is named for where it comes from.
        if *count > 1 {
            format!("{}_{rel_type}", type_for(from))
        } else {
            rel_type
        }
    };

    for table in &node_tables {
        mapping.nodes.push(NodeMap {
            table: Some(table.name.clone()),
            query: None,
            label: label_for(&table.name),
            id: Columns::from_list(table.primary_key.clone()),
            columns: None,
            filter: None,
            updated_at: updated(table),
        });
    }
    for table in &node_tables {
        for key in &table.foreign_keys {
            if !points_at_key(key) || !is_node(&key.ref_table) {
                notes.push(format!(
                    "{}: the foreign key on ({}) does not point at the primary key of a node table, and was left out",
                    table.name,
                    key.columns.join(", ")
                ));
                continue;
            }
            mapping.edges.push(EdgeMap {
                table: Some(table.name.clone()),
                query: None,
                rel_type: unique(type_for_key(key), &table.name),
                from: End {
                    label: label_for(&table.name),
                    columns: Columns::from_list(table.primary_key.clone()),
                },
                to: End {
                    label: label_for(&key.ref_table),
                    columns: Columns::from_list(key.columns.clone()),
                },
                // One relationship per row: when the key is pointed somewhere
                // else, it is the same relationship, moved.
                id: Some(Columns::from_list(table.primary_key.clone())),
                columns: Some(Vec::new()),
                filter: None,
                updated_at: updated(table),
            });
        }
    }
    for table in tables.iter().filter(|t| is_join(t)) {
        // The direction follows the table's own column order, not the order
        // the database happens to list its keys in.
        let position = |key: &ForeignKey| table.columns.iter().position(|c| key.columns.first().is_some_and(|k| k.eq_ignore_ascii_case(c)));
        let (mut a, mut b) = (&table.foreign_keys[0], &table.foreign_keys[1]);
        if position(b) < position(a) {
            std::mem::swap(&mut a, &mut b);
        }
        if !is_node(&a.ref_table) || !is_node(&b.ref_table) {
            notes.push(format!("{}: joins a table that is not a node table, and was left out", table.name));
            continue;
        }
        let ends: Vec<&String> = a.columns.iter().chain(&b.columns).collect();
        let properties: Vec<String> = table.columns.iter().filter(|c| !ends.iter().any(|e| e.eq_ignore_ascii_case(c))).cloned().collect();
        mapping.edges.push(EdgeMap {
            table: Some(table.name.clone()),
            query: None,
            rel_type: unique(type_for(&table.name), &table.name),
            from: End {
                label: label_for(&a.ref_table),
                columns: Columns::from_list(a.columns.clone()),
            },
            to: End {
                label: label_for(&b.ref_table),
                columns: Columns::from_list(b.columns.clone()),
            },
            id: None,
            columns: Some(properties),
            filter: None,
            updated_at: updated(table),
        });
    }
    for table in tables.iter().filter(|t| t.primary_key.is_empty() && !is_join(t)) {
        notes.push(format!(
            "{}: no primary key, so its rows can't be told apart; map it by hand in `nodes` or `edges` to read it",
            table.name
        ));
    }
    (mapping, notes)
}

/// Where records and what is said about the run go.
pub trait Sink {
    fn node(&mut self, record: NodeRecord) -> Result<(), String>;
    fn edge(&mut self, record: EdgeRecord) -> Result<(), String>;
    fn progress(&mut self, message: &str, current: u64, total: u64) -> Result<(), String>;
    fn log(&mut self, level: &str, message: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Read only rows changed since `cursor`, in tables that have `updated_at`.
    pub incremental: bool,
    /// What the last sync reported as its cursor.
    pub cursor: Option<Value>,
    /// The most rows read from one table; 0 for no limit. Past it the run is
    /// incomplete, and says so.
    pub row_limit: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub nodes: u64,
    pub edges: u64,
    /// Where the next incremental sync starts.
    pub cursor: Value,
    /// What was not read. Not empty means the run did not cover the source.
    pub gaps: Vec<String>,
}

fn quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A row's value under `column`, whatever case the database returned it in.
fn cell<'a>(row: &'a Row, column: &str) -> Option<&'a Value> {
    row.get(column)
        .or_else(|| row.iter().find(|(key, _)| key.eq_ignore_ascii_case(column)).map(|(_, value)| value))
}

/// The key a row has in `columns`, joined with `:`; `None` when part of it is null.
fn key_of(row: &Row, columns: &[String]) -> Option<String> {
    let mut parts = Vec::with_capacity(columns.len());
    for column in columns {
        parts.push(match cell(row, column)? {
            Value::Null => return None,
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    }
    Some(parts.join(":"))
}

fn node_id(label: &str, key: &str) -> String {
    format!("{label}:{key}")
}

/// What one table's read needs: where from, which rows, and its cursor.
struct Read<'a> {
    what: String,
    cursor_key: String,
    table: &'a Option<String>,
    query: &'a Option<String>,
    select: Option<Vec<String>>,
    filter: &'a Option<String>,
    updated_at: &'a Option<String>,
}

struct Runner<'a> {
    driver: &'a mut dyn Driver,
    options: &'a Options,
    last: Map<String, Value>,
    next: Map<String, Value>,
    gaps: Vec<String>,
}

impl Runner<'_> {
    fn source(&self, read: &Read) -> String {
        match (read.table, read.query) {
            (Some(table), _) => self.driver.table_ref(table),
            (None, Some(query)) => format!("({}) AS khonsu_q", query.trim().trim_end_matches(';')),
            (None, None) => unreachable!("validated"),
        }
    }

    /// Reads the rows of one mapped table. Returns how many were handed on.
    fn read(&mut self, read: &Read, on_row: &mut dyn FnMut(&Row) -> Result<(), String>) -> Result<u64, String> {
        let source = self.source(read);
        let mut conditions: Vec<String> = read.filter.iter().map(|filter| format!("({filter})")).collect();
        if let Some(column) = read.updated_at {
            let quoted = self.driver.quote(column);
            // The high-water mark is read first, so a row that changes during
            // the read is picked up next time rather than missed.
            let mut high = None;
            let sql = format!(
                "SELECT {} AS khonsu_high FROM {source}{}",
                self.driver.as_text(&format!("max({quoted})")),
                match read.filter {
                    Some(filter) => format!(" WHERE ({filter})"),
                    None => String::new(),
                }
            );
            self.driver
                .rows(&sql, &mut |row| {
                    high = row.values().next().and_then(Value::as_str).map(str::to_string);
                    Ok(())
                })
                .map_err(|e| format!("{}: reading the newest {column}: {e}", read.what))?;
            let last = self.last.get(&read.cursor_key).and_then(Value::as_str).map(str::to_string);
            if self.options.incremental {
                if let Some(last) = &last {
                    conditions.push(format!("{quoted} > {}", quote_text(last)));
                }
                if let Some(high) = &high {
                    conditions.push(format!("{quoted} <= {}", quote_text(high)));
                }
            }
            if let Some(mark) = high.or(last) {
                self.next.insert(read.cursor_key.clone(), Value::String(mark));
            }
        }
        let columns = match &read.select {
            Some(columns) => columns.iter().map(|c| self.driver.quote(c)).collect::<Vec<_>>().join(", "),
            None => "*".into(),
        };
        let mut sql = format!("SELECT {columns} FROM {source}");
        if !conditions.is_empty() {
            sql.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
        }
        let limit = self.options.row_limit;
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit + 1));
        }
        let mut seen = 0u64;
        let mut over = false;
        self.driver
            .rows(&sql, &mut |row| {
                if limit > 0 && seen >= limit {
                    over = true;
                    return Ok(());
                }
                seen += 1;
                on_row(&row)
            })
            .map_err(|e| format!("{}: {e}", read.what))?;
        if over {
            self.gaps.push(format!("{}: more than {limit} rows; only the first {limit} were read", read.what));
        }
        Ok(seen)
    }
}

/// The columns to select: the keys and the properties, or everything.
fn selection(keys: &[&[String]], properties: &Option<Vec<String>>, extra: &Option<String>) -> Option<Vec<String>> {
    let properties = properties.as_ref()?;
    let mut columns: Vec<String> = Vec::new();
    let all = keys.iter().flat_map(|k| k.iter()).chain(properties.iter()).chain(extra.iter());
    for column in all {
        if !columns.iter().any(|c| c.eq_ignore_ascii_case(column)) {
            columns.push(column.clone());
        }
    }
    Some(columns)
}

/// A row's properties: the columns asked for, or all but `without`. Nulls are
/// left out, as the host expects.
fn properties(row: &Row, wanted: &Option<Vec<String>>, without: &[String]) -> Map<String, Value> {
    let mut out = Map::new();
    match wanted {
        Some(wanted) => {
            for column in wanted {
                if let Some(value) = cell(row, column).filter(|v| !v.is_null()) {
                    out.insert(column.clone(), value.clone());
                }
            }
        }
        None => {
            for (column, value) in row {
                if !value.is_null() && !without.iter().any(|w| w.eq_ignore_ascii_case(column)) {
                    out.insert(column.clone(), value.clone());
                }
            }
        }
    }
    out
}

/// Reads every mapped table and reports its rows as nodes and relationships.
pub fn run(driver: &mut dyn Driver, mapping: &Mapping, options: &Options, sink: &mut dyn Sink) -> Result<Outcome, String> {
    mapping.validate()?;
    let last = options
        .cursor
        .as_ref()
        .and_then(|cursor| cursor.get("tables"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut runner = Runner {
        driver,
        options,
        // What a table is not read for this time keeps its mark.
        next: last.clone(),
        last,
        gaps: Vec::new(),
    };
    let total = (mapping.nodes.len() + mapping.edges.len()) as u64;
    let mut step = 0u64;
    let mut outcome = Outcome::default();

    for node in &mapping.nodes {
        step += 1;
        let what = format!("{} ({})", node.label, node.table.as_deref().unwrap_or("query"));
        sink.progress(&what, step, total)?;
        let id = node.id.list();
        let read = Read {
            what: what.clone(),
            cursor_key: format!("n:{}", node.label),
            table: &node.table,
            query: &node.query,
            select: selection(&[&id], &node.columns, &None),
            filter: &node.filter,
            updated_at: &node.updated_at,
        };
        let mut without_key = 0u64;
        let mut failed = None;
        let seen = runner.read(&read, &mut |row| {
            let Some(key) = key_of(row, &id) else {
                without_key += 1;
                return Ok(());
            };
            let record = NodeRecord {
                id: node_id(&node.label, &key),
                label: node.label.clone(),
                properties: properties(row, &node.columns, &[]),
            };
            sink.node(record).inspect_err(|e| failed = Some(e.clone()))
        });
        let seen = seen.map_err(|e| failed.take().unwrap_or(e))?;
        outcome.nodes += seen - without_key;
        if without_key > 0 {
            sink.log("warn", &format!("{what}: {without_key} row(s) with no value in ({}) were skipped", id.join(", ")))?;
        }
    }

    for edge in &mapping.edges {
        step += 1;
        let what = format!("{} ({})", edge.rel_type, edge.table.as_deref().unwrap_or("query"));
        sink.progress(&what, step, total)?;
        let (from, to) = (edge.from.columns.list(), edge.to.columns.list());
        let id = edge.id.as_ref().map(Columns::list);
        let mut keys: Vec<&[String]> = vec![&from, &to];
        if let Some(id) = &id {
            keys.push(id);
        }
        let read = Read {
            what: what.clone(),
            cursor_key: format!("e:{}:{}:{}", edge.rel_type, edge.from.label, edge.to.label),
            table: &edge.table,
            query: &edge.query,
            select: selection(&keys, &edge.columns, &edge.updated_at),
            filter: &edge.filter,
            updated_at: &edge.updated_at,
        };
        let ends: Vec<String> = from.iter().chain(&to).cloned().collect();
        let mut open_ended = 0u64;
        let mut failed = None;
        let seen = runner.read(&read, &mut |row| {
            let (Some(source), Some(target)) = (key_of(row, &from), key_of(row, &to)) else {
                // A foreign key left empty: the row points at nothing.
                open_ended += 1;
                return Ok(());
            };
            let source = node_id(&edge.from.label, &source);
            let target = node_id(&edge.to.label, &target);
            let record = EdgeRecord {
                id: match id.as_ref().and_then(|id| key_of(row, id)) {
                    Some(key) => format!("{}:{}:{key}", edge.rel_type, edge.from.label),
                    None => format!("{source}>{}>{target}", edge.rel_type),
                },
                source,
                target,
                rel_type: edge.rel_type.clone(),
                properties: properties(row, &edge.columns, &ends),
            };
            sink.edge(record).inspect_err(|e| failed = Some(e.clone()))
        });
        let seen = seen.map_err(|e| failed.take().unwrap_or(e))?;
        outcome.edges += seen - open_ended;
    }

    outcome.cursor = json!({ "tables": runner.next });
    outcome.gaps = runner.gaps;
    Ok(outcome)
}

/// A mapping in a few words, for a connection test.
pub fn summary(mapping: &Mapping) -> String {
    let nodes: Vec<String> = mapping.nodes.iter().map(|n| n.label.clone()).collect();
    let edges: Vec<String> = mapping
        .edges
        .iter()
        .map(|e| format!("(:{})-[:{}]->(:{})", e.from.label, e.rel_type, e.to.label))
        .collect();
    format!(
        "{} node table(s): {}. {} relationship(s): {}",
        nodes.len(),
        if nodes.is_empty() { "none".into() } else { nodes.join(", ") },
        edges.len(),
        if edges.is_empty() { "none".into() } else { edges.join(", ") }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, columns: &[&str], key: &[&str], foreign: &[(&[&str], &str, &[&str])]) -> Table {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Table {
            name: name.into(),
            columns: list(columns),
            primary_key: list(key),
            foreign_keys: foreign
                .iter()
                .map(|(columns, ref_table, ref_columns)| ForeignKey {
                    columns: list(columns),
                    ref_table: ref_table.to_string(),
                    ref_columns: list(ref_columns),
                })
                .collect(),
        }
    }

    fn shop() -> Vec<Table> {
        vec![
            table("customers", &["id", "name", "updated_at"], &["id"], &[]),
            table("products", &["id", "name"], &["id"], &[]),
            table(
                "orders",
                &["id", "customer_id", "updated_at"],
                &["id"],
                &[(&["customer_id"], "customers", &["id"])],
            ),
            table(
                "order_items",
                &["order_id", "product_id", "qty"],
                &["order_id", "product_id"],
                &[(&["order_id"], "orders", &["id"]), (&["product_id"], "products", &["id"])],
            ),
            table("audit_log", &["at", "what"], &[], &[]),
        ]
    }

    #[test]
    fn keys_become_nodes_relationships_and_join_tables() {
        let (mapping, notes) = infer(&shop(), Some("updated_at"));
        assert_eq!(mapping.labels(), ["Customers", "Orders", "Products"]);
        let edges: Vec<String> = mapping
            .edges
            .iter()
            .map(|e| format!("{}-{}->{}", e.from.label, e.rel_type, e.to.label))
            .collect();
        assert_eq!(edges, ["Orders-CUSTOMER->Customers", "Orders-ORDER_ITEMS->Products"]);
        // The join table's own column is the relationship's property.
        assert_eq!(mapping.edges[1].columns, Some(vec!["qty".to_string()]));
        assert_eq!(mapping.nodes[0].updated_at.as_deref(), Some("updated_at"));
        assert_eq!(mapping.nodes[1].updated_at, None);
        assert!(notes.iter().any(|n| n.starts_with("audit_log: no primary key")), "{notes:?}");
        mapping.validate().unwrap();
    }

    #[test]
    fn names_follow_the_graph_conventions() {
        assert_eq!(label_for("order_items"), "OrderItems");
        assert_eq!(label_for("2fa"), "T2fa");
        assert_eq!(type_for("orderItems"), "ORDER_ITEMS");
        assert_eq!(type_for("order-items"), "ORDER_ITEMS");
        let key = |column: &str| ForeignKey {
            columns: vec![column.into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
        };
        assert_eq!(type_for_key(&key("manager_id")), "MANAGER");
        assert_eq!(type_for_key(&key("ownerId")), "OWNER");
        assert_eq!(type_for_key(&key("id")), "USERS");
    }

    /// A driver over rows held in memory, recording the SQL it was asked for.
    struct Fake {
        asked: Vec<String>,
        rows: Vec<(&'static str, Vec<Row>)>,
    }

    impl Driver for Fake {
        fn catalog(&mut self) -> Result<Vec<Table>, String> {
            Ok(shop())
        }
        fn quote(&self, name: &str) -> String {
            format!("\"{name}\"")
        }
        fn as_text(&self, expr: &str) -> String {
            format!("CAST({expr} AS TEXT)")
        }
        fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String> {
            self.asked.push(sql.to_string());
            if sql.contains("khonsu_high") {
                return on_row(json!({"khonsu_high": "2026-11-02"}).as_object().unwrap().clone());
            }
            let found = self.rows.iter().find(|(table, _)| sql.contains(&format!("FROM \"{table}\"")));
            for row in found.map(|(_, rows)| rows.clone()).unwrap_or_default() {
                on_row(row)?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Collected {
        nodes: Vec<NodeRecord>,
        edges: Vec<EdgeRecord>,
        logs: Vec<String>,
    }

    impl Sink for Collected {
        fn node(&mut self, record: NodeRecord) -> Result<(), String> {
            self.nodes.push(record);
            Ok(())
        }
        fn edge(&mut self, record: EdgeRecord) -> Result<(), String> {
            self.edges.push(record);
            Ok(())
        }
        fn progress(&mut self, _: &str, _: u64, _: u64) -> Result<(), String> {
            Ok(())
        }
        fn log(&mut self, _: &str, message: &str) -> Result<(), String> {
            self.logs.push(message.to_string());
            Ok(())
        }
    }

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn rows_become_records_and_the_cursor_bounds_the_next_read() {
        let (mapping, _) = infer(&shop(), Some("updated_at"));
        let mut driver = Fake {
            asked: Vec::new(),
            rows: vec![
                ("customers", vec![row(json!({"id": 1, "name": "Ada", "updated_at": "2026-11-01"}))]),
                ("products", vec![row(json!({"id": 7, "name": "Lamp"}))]),
                (
                    "orders",
                    vec![
                        row(json!({"id": 10, "customer_id": 1, "updated_at": "2026-11-02"})),
                        row(json!({"id": 11, "customer_id": null, "updated_at": "2026-11-02"})),
                    ],
                ),
                ("order_items", vec![row(json!({"order_id": 10, "product_id": 7, "qty": 2}))]),
            ],
        };
        let mut sink = Collected::default();
        let outcome = run(&mut driver, &mapping, &Options::default(), &mut sink).unwrap();
        assert_eq!((outcome.nodes, outcome.edges), (4, 2));
        assert_eq!(sink.nodes[0].id, "Customers:1");
        assert_eq!(sink.nodes[0].properties["name"], "Ada");
        // The order with no customer makes a node and no relationship.
        let placed: Vec<&EdgeRecord> = sink.edges.iter().filter(|e| e.rel_type == "CUSTOMER").collect();
        assert_eq!(placed.len(), 1);
        assert_eq!((placed[0].source.as_str(), placed[0].target.as_str()), ("Orders:10", "Customers:1"));
        let item = sink.edges.iter().find(|e| e.rel_type == "ORDER_ITEMS").unwrap();
        assert_eq!(item.properties, row(json!({"qty": 2})));
        assert_eq!(outcome.cursor["tables"]["n:Customers"], "2026-11-02");

        // The next incremental sync reads only what changed since.
        driver.asked.clear();
        let options = Options {
            incremental: true,
            cursor: Some(outcome.cursor),
            row_limit: 0,
        };
        run(&mut driver, &mapping, &options, &mut Collected::default()).unwrap();
        let customers = driver.asked.iter().find(|sql| sql.starts_with("SELECT * FROM \"customers\"")).unwrap();
        assert!(
            customers.ends_with("WHERE \"updated_at\" > '2026-11-02' AND \"updated_at\" <= '2026-11-02'"),
            "{customers}"
        );
        // A table with no such column is read whole.
        assert!(driver.asked.iter().any(|sql| sql == "SELECT * FROM \"products\""));
    }

    #[test]
    fn a_row_limit_is_reported_as_a_gap() {
        let mapping: Mapping = serde_json::from_value(json!({
            "nodes": [{"table": "products", "label": "Product", "id": "id"}]
        }))
        .unwrap();
        let mut driver = Fake {
            asked: Vec::new(),
            rows: vec![("products", (0..5).map(|i| row(json!({"id": i}))).collect())],
        };
        let mut sink = Collected::default();
        let options = Options {
            row_limit: 3,
            ..Options::default()
        };
        let outcome = run(&mut driver, &mapping, &options, &mut sink).unwrap();
        assert_eq!(sink.nodes.len(), 3);
        assert_eq!(outcome.gaps.len(), 1);
        assert!(driver.asked[0].ends_with("LIMIT 4"));
    }

    #[test]
    fn a_mapping_that_cannot_work_is_refused_before_the_database() {
        let parse = |value: Value| mapping_from_config(value.as_object().unwrap());
        assert_eq!(parse(json!({})).unwrap(), None);
        let unknown = parse(json!({
            "nodes": [{"table": "a", "label": "A", "id": "id"}],
            "edges": [{"table": "ab", "type": "T", "from": {"label": "A", "column": "a"}, "to": {"label": "B", "column": "b"}}]
        }));
        assert!(unknown.unwrap_err().contains("\"B\" is not the label"));
        let arity = parse(json!({
            "nodes": [{"table": "a", "label": "A", "id": ["x", "y"]}],
            "edges": [{"table": "aa", "type": "T", "from": {"label": "A", "column": "a"}, "to": {"label": "A", "columns": ["x", "y"]}}]
        }));
        assert!(arity.unwrap_err().contains("identified by 2"));
        // JSON given as text, as the window's form sends it.
        let text = parse(json!({"nodes": "[{\"query\": \"SELECT 1 AS id\", \"label\": \"One\", \"id\": \"id\"}]"}));
        assert_eq!(text.unwrap().unwrap().nodes[0].label, "One");
    }
}
