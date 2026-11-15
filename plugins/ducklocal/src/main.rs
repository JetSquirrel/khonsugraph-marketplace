//! The DuckLocal data source: data files (Parquet, CSV, JSON, Excel) and
//! DuckDB databases as nodes and relationships.
//!
//! DuckLocal is the reader: this plugin runs `ducklocal query` with the SQL
//! of a mapping and turns the rows into records (see `khonsu_tables`). So
//! whatever DuckLocal can read, a graph can be made from, and the query a
//! person worked out in DuckLocal is the query a mapping takes.

use khonsu_plugin_protocol::{
    read_request, ConfigField, Describe, EdgeRecord, Emitter, FieldKind, Message, NodeRecord, Permissions, Request, SyncMode,
};
use khonsu_tables::{infer, mapping_from_config, run, summary, Driver, ForeignKey, Mapping, Options, Row, Sink, Table};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

const DEFAULT_ROW_LIMIT: u64 = 1_000_000;
const DEFAULT_INCREMENTAL_COLUMN: &str = "updated_at";
/// Rows asked for at a time. DuckLocal prints at most 2,000,000 cells, which
/// this stays under for a table of up to 200 columns.
const PAGE: u64 = 10_000;

fn describe() -> Describe {
    let field = |key: &str, label: &str, kind: FieldKind, help: &str| ConfigField {
        key: key.into(),
        label: label.into(),
        kind,
        required: false,
        secret: false,
        help: help.into(),
        default: None,
    };
    Describe {
        name: "DuckLocal".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Parquet, CSV, JSON and DuckDB tables as nodes and relationships, read through DuckLocal. A DuckDB database is mapped from its primary and foreign keys; files need a mapping, since they have no keys.".into(),
        config: vec![
            field("database", "DuckDB file", FieldKind::File, "A DuckDB database, opened read-only. Leave empty to read data files"),
            field(
                "folder",
                "Folder",
                FieldKind::File,
                "The folder that file names in the mapping are read from, such as 'orders.parquet'",
            ),
            field(
                "tables",
                "Tables",
                FieldKind::String,
                "Only these tables of the database, comma-separated, when the mapping is inferred",
            ),
            field(
                "nodes",
                "Node tables",
                FieldKind::Json,
                r#"[{"table" | "query", "label", "id", "columns", "where", "updated_at"}]; "table" is a table of the database, or a file ('customers.parquet', 'logs/*.csv'); "id" is the key column, or a list of them"#,
            ),
            field(
                "edges",
                "Edge tables",
                FieldKind::Json,
                r#"[{"table" | "query", "type", "from": {"label", "column"}, "to": {"label", "column"}, "id", "columns", "where", "updated_at"}]; each end names a node table's label and the column here that holds its key"#,
            ),
            ConfigField {
                default: Some(Value::String(DEFAULT_INCREMENTAL_COLUMN.into())),
                ..field(
                    "incremental_column",
                    "Incremental column",
                    FieldKind::String,
                    "When the mapping is inferred: a column that only grows as rows change. Empty turns this off",
                )
            },
            ConfigField {
                default: Some(Value::from(DEFAULT_ROW_LIMIT)),
                ..field(
                    "row_limit",
                    "Row limit per table",
                    FieldKind::Number,
                    "A table with more rows is read up to here and the sync is marked partial; 0 for no limit",
                )
            },
            field("ducklocal", "DuckLocal command", FieldKind::File, "The ducklocal program, when it is not on PATH"),
        ],
        permissions: Permissions {
            read_files: true,
            commands: vec!["ducklocal".into()],
            notes: vec![
                "Runs `ducklocal query` with the SQL of your mapping; a DuckDB file is opened read-only".into(),
                "The SQL you write can read whatever DuckDB can reach, including URLs".into(),
            ],
            ..Permissions::default()
        },
        labels: Vec::new(),
        relationship_types: Vec::new(),
        supports_incremental: true,
        analyses: Vec::new(),
    }
}

struct Config {
    program: String,
    database: Option<String>,
    folder: Option<PathBuf>,
    tables: Vec<String>,
    mapping: Option<Mapping>,
    incremental_column: Option<String>,
    row_limit: u64,
}

fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn parse_config(request: &Request) -> Result<Config, String> {
    let c = &request.config;
    let row_limit = match c.get("row_limit") {
        None | Some(Value::Null) => DEFAULT_ROW_LIMIT,
        Some(Value::String(s)) if s.trim().is_empty() => DEFAULT_ROW_LIMIT,
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or("row_limit must be a whole number, 0 for no limit")?,
    };
    let config = Config {
        program: text(c, "ducklocal").unwrap_or_else(|| "ducklocal".into()),
        database: text(c, "database"),
        folder: text(c, "folder").map(PathBuf::from),
        tables: text(c, "tables")
            .map(|list| list.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect())
            .unwrap_or_default(),
        mapping: mapping_from_config(c)?,
        incremental_column: match c.get("incremental_column") {
            None | Some(Value::Null) => Some(DEFAULT_INCREMENTAL_COLUMN.to_string()),
            Some(_) => text(c, "incremental_column"),
        },
        row_limit,
    };
    if config.database.is_none() && config.mapping.is_none() {
        return Err("data files have no keys to infer a mapping from: say which are nodes and which are relationships in `nodes` and `edges`, or give a DuckDB file".into());
    }
    if let Some(folder) = &config.folder {
        if !folder.is_dir() {
            return Err(format!("{}: no such folder", folder.display()));
        }
    }
    Ok(config)
}

/// A value as DuckLocal prints it, as a property. Types JSON has no word for
/// arrive as `{"encoding": …, "value": …, "iso": …}`.
fn plain(value: Value) -> Value {
    match value {
        Value::Object(map) if map.contains_key("encoding") => {
            let encoding = map.get("encoding").and_then(Value::as_str).unwrap_or_default();
            let word = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_string);
            match encoding {
                "decimal" => word("value")
                    .and_then(|text| text.parse::<f64>().ok())
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .or_else(|| word("value").map(Value::String))
                    .unwrap_or(Value::Null),
                // Bytes have no place in a property.
                "binary" | "blob" | "base64" => Value::Null,
                _ => word("iso").or_else(|| word("value")).map(Value::String).unwrap_or(Value::Null),
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, plain(value))).collect()),
        other => other,
    }
}

struct Duck {
    program: String,
    database: Option<String>,
    folder: Option<PathBuf>,
}

impl Duck {
    /// Runs one statement and returns DuckLocal's JSON.
    fn query(&self, sql: &str, limit: u64) -> Result<Value, String> {
        let mut command = Command::new(&self.program);
        command.arg("query").arg("--sql").arg(sql).arg("--limit").arg(limit.to_string());
        if let Some(database) = &self.database {
            command.arg("--database").arg(database);
        }
        if let Some(folder) = &self.folder {
            command.current_dir(folder);
        }
        let output = command.stdin(Stdio::null()).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{} was not found: install DuckLocal and put its command on PATH (its title bar has a button for that), or set the DuckLocal command",
                    self.program
                )
            } else {
                format!("{}: {e}", self.program)
            }
        })?;
        if !output.status.success() {
            // Errors are JSON on stderr: {"error": {"kind", "message", "hint"}}.
            let said = String::from_utf8_lossy(&output.stderr);
            let error = said.lines().rev().find_map(|line| serde_json::from_str::<Value>(line).ok());
            return Err(match error.as_ref().and_then(|e| e.get("error")) {
                Some(error) => {
                    let word = |key: &str| error.get(key).and_then(Value::as_str).unwrap_or_default();
                    match word("hint") {
                        "" => word("message").to_string(),
                        hint => format!("{} ({hint})", word("message")),
                    }
                }
                None => said.trim().to_string(),
            });
        }
        serde_json::from_slice(&output.stdout).map_err(|e| format!("what ducklocal printed is not JSON: {e}"))
    }

    fn version(&self) -> Result<String, String> {
        let output = Command::new(&self.program).arg("--version").output().map_err(|e| format!("{}: {e}", self.program))?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

fn names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

impl Driver for Duck {
    fn catalog(&mut self) -> Result<Vec<Table>, String> {
        let mut tables: Vec<Table> = Vec::new();
        self.rows(
            "SELECT table_name AS name, list(column_name ORDER BY ordinal_position) AS columns \
             FROM information_schema.columns WHERE table_schema = current_schema() AND table_catalog = current_database() \
             GROUP BY table_name ORDER BY table_name",
            &mut |row| {
                tables.push(Table {
                    name: row.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    columns: names(row.get("columns")),
                    ..Table::default()
                });
                Ok(())
            },
        )?;
        let mut keys = Vec::new();
        self.rows(
            "SELECT table_name AS name, constraint_type AS kind, constraint_column_names AS columns, \
             referenced_table AS ref_table, referenced_column_names AS ref_columns \
             FROM duckdb_constraints() WHERE schema_name = current_schema() AND database_name = current_database() \
             AND constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY') ORDER BY table_name, constraint_index",
            &mut |row| {
                keys.push(row);
                Ok(())
            },
        )?;
        for key in keys {
            let word = |name: &str| key.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
            let Some(table) = tables.iter_mut().find(|t| t.name == word("name")) else {
                continue;
            };
            if word("kind") == "PRIMARY KEY" {
                table.primary_key = names(key.get("columns"));
            } else {
                table.foreign_keys.push(ForeignKey {
                    columns: names(key.get("columns")),
                    ref_table: word("ref_table"),
                    ref_columns: names(key.get("ref_columns")),
                });
            }
        }
        Ok(tables)
    }

    fn quote(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// A table of the database by name; anything that looks like a file, or
    /// any name when there is no database, as a file DuckDB reads.
    fn table_ref(&self, table: &str) -> String {
        let file = table.contains(['/', '\\', '.', '*']) || self.database.is_none();
        if file {
            format!("'{}'", table.replace('\'', "''"))
        } else {
            self.quote(table)
        }
    }

    fn as_text(&self, expr: &str) -> String {
        format!("CAST({expr} AS VARCHAR)")
    }

    fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String> {
        let mut offset = 0u64;
        loop {
            // DuckDB keeps the order rows were stored in, so pages line up.
            let page = format!("SELECT * FROM ({sql}) AS khonsu_p LIMIT {PAGE} OFFSET {offset}");
            let result = self.query(&page, PAGE)?;
            let columns: Vec<String> = result
                .get("columns")
                .and_then(Value::as_array)
                .map(|columns| columns.iter().map(|c| c.get("name").and_then(Value::as_str).unwrap_or_default().to_string()).collect())
                .unwrap_or_default();
            let Some(Value::Array(rows)) = result.get("rows").cloned() else {
                return Err("ducklocal printed no rows".into());
            };
            // A page cut short by the cell limit would lose rows without a word.
            if result.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
                return Err(format!(
                    "a page of {PAGE} rows is more than ducklocal prints at once ({} columns): keep fewer with `columns`",
                    columns.len()
                ));
            }
            let count = rows.len() as u64;
            for row in rows {
                let Value::Array(cells) = row else {
                    return Err("a row ducklocal printed is not a list".into());
                };
                on_row(columns.iter().cloned().zip(cells.into_iter().map(plain)).collect())?;
            }
            if count < PAGE {
                return Ok(());
            }
            offset += count;
        }
    }
}

fn open(config: &Config) -> Duck {
    Duck {
        program: config.program.clone(),
        database: config.database.clone(),
        folder: config.folder.clone(),
    }
}

fn mapping_for(config: &Config, duck: &mut Duck) -> Result<(Mapping, Vec<String>, bool), String> {
    if let Some(mapping) = &config.mapping {
        return Ok((mapping.clone(), Vec::new(), false));
    }
    let mut tables = duck.catalog().map_err(|e| format!("reading the catalog: {e}"))?;
    if !config.tables.is_empty() {
        for wanted in &config.tables {
            if !tables.iter().any(|t| t.name.eq_ignore_ascii_case(wanted)) {
                let known: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
                return Err(format!("no table {wanted:?}; tables: {}", known.join(", ")));
            }
        }
        tables.retain(|t| config.tables.iter().any(|wanted| t.name.eq_ignore_ascii_case(wanted)));
    }
    let (mapping, notes) = infer(&tables, config.incremental_column.as_deref());
    if mapping.is_empty() {
        return Err(format!(
            "nothing could be inferred from {} table(s): none has a primary key. Map them by hand in `nodes` and `edges`",
            tables.len()
        ));
    }
    Ok((mapping, notes, true))
}

fn test(config: &Config) -> Result<String, String> {
    let mut duck = open(config);
    let version = duck.version()?;
    let (mapping, notes, inferred) = mapping_for(config, &mut duck)?;
    // Each mapped table is read for one row, so a wrong path shows now.
    for (what, table, query) in mapping
        .nodes
        .iter()
        .map(|n| (&n.label, &n.table, &n.query))
        .chain(mapping.edges.iter().map(|e| (&e.rel_type, &e.table, &e.query)))
    {
        let from = match (table, query) {
            (Some(table), _) => duck.table_ref(table),
            (None, Some(query)) => format!("({}) AS khonsu_q", query.trim().trim_end_matches(';')),
            (None, None) => continue,
        };
        duck.query(&format!("SELECT * FROM {from} LIMIT 1"), 1).map_err(|e| format!("{what}: {e}"))?;
    }
    let mut message = format!("{version}. {}{}", if inferred { "Inferred from keys: " } else { "" }, summary(&mapping));
    if !notes.is_empty() {
        message.push_str(&format!(". Left out: {}", notes.join("; ")));
    }
    Ok(message)
}

struct Out<'a, W: Write>(&'a mut Emitter<W>);

impl<W: Write> Sink for Out<'_, W> {
    fn node(&mut self, record: NodeRecord) -> Result<(), String> {
        self.0.node(record).map_err(|e| e.to_string())
    }
    fn edge(&mut self, record: EdgeRecord) -> Result<(), String> {
        self.0.edge(record).map_err(|e| e.to_string())
    }
    fn progress(&mut self, message: &str, current: u64, total: u64) -> Result<(), String> {
        self.0.progress(message, Some(current), Some(total)).map_err(|e| e.to_string())
    }
    fn log(&mut self, level: &str, message: &str) -> Result<(), String> {
        self.0
            .emit(&Message::Log {
                level: level.into(),
                message: message.into(),
            })
            .map_err(|e| e.to_string())
    }
}

fn sync<W: Write>(request: &Request, config: &Config, out: &mut Emitter<W>) -> Result<(), String> {
    let mut duck = open(config);
    let (mapping, notes, inferred) = mapping_for(config, &mut duck)?;
    let mut sink = Out(out);
    if inferred {
        let written = serde_json::to_string(&mapping).map_err(|e| e.to_string())?;
        sink.log("info", &format!("mapping inferred from keys: {written}"))?;
    }
    for note in &notes {
        sink.log("warn", note)?;
    }
    let options = Options {
        incremental: request.mode == SyncMode::Incremental,
        cursor: request.cursor.clone(),
        row_limit: config.row_limit,
    };
    let outcome = run(&mut duck, &mapping, &options, &mut sink)?;
    out.emit(&Message::Cursor { value: outcome.cursor }).map_err(|e| e.to_string())?;
    out.emit(&Message::Done {
        nodes: outcome.nodes,
        edges: outcome.edges,
        complete: outcome.gaps.is_empty(),
        gaps: outcome.gaps,
    })
    .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let mut out = Emitter::stdout();
    let command = std::env::args().nth(1).unwrap_or_default();
    let result = match command.as_str() {
        "describe" => out.emit(&Message::Describe(describe())).map_err(|e| e.to_string()),
        "test" => read_request()
            .map_err(|e| e.to_string())
            .and_then(|r| parse_config(&r))
            .and_then(|c| test(&c))
            .and_then(|message| out.emit(&Message::TestResult { ok: true, message }).map_err(|e| e.to_string()))
            .or_else(|message| out.emit(&Message::TestResult { ok: false, message }).map_err(|e| e.to_string())),
        "sync" => read_request()
            .map_err(|e| e.to_string())
            .and_then(|r| parse_config(&r).map(|c| (r, c)))
            .and_then(|(r, c)| sync(&r, &c, &mut out)),
        other => Err(format!("unknown command {other:?}; expected describe, test or sync")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = out.emit(&Message::Error { message: message.clone() });
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_ducklocal_encodes_becomes_a_plain_property() {
        assert_eq!(plain(json!({"encoding": "decimal", "value": "19.50"})), json!(19.5));
        assert_eq!(
            plain(json!({"encoding": "timestamp", "unit": "Microsecond", "value": "1793527200000000", "iso": "2026-11-01T10:00:00"})),
            json!("2026-11-01T10:00:00")
        );
        assert_eq!(plain(json!([{"encoding": "date", "value": "20758", "iso": "2026-11-01"}, 2])), json!(["2026-11-01", 2]));
        assert_eq!(plain(json!({"w": 40})), json!({"w": 40}));
    }

    #[test]
    fn files_need_a_mapping_and_a_database_does_not() {
        let request = |config: Value| Request {
            protocol: 1,
            config: config.as_object().unwrap().clone(),
            ..Request::default()
        };
        let error = parse_config(&request(json!({}))).err().unwrap();
        assert!(error.contains("no keys to infer"), "{error}");
        assert!(parse_config(&request(json!({"database": "shop.duckdb"}))).is_ok());
        let files = parse_config(&request(json!({"nodes": [{"table": "a.parquet", "label": "A", "id": "id"}]}))).unwrap();
        let duck = open(&files);
        // With no database, a table is a file.
        assert_eq!(duck.table_ref("customers"), "'customers'");
        assert_eq!(duck.table_ref("logs/*.csv"), "'logs/*.csv'");
        let duck = open(&parse_config(&request(json!({"database": "shop.duckdb"}))).unwrap());
        assert_eq!(duck.table_ref("customers"), "\"customers\"");
        assert_eq!(duck.table_ref("extra.parquet"), "'extra.parquet'");
    }
}
