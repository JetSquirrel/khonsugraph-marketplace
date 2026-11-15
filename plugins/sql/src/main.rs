//! The SQL data source: tables of a PostgreSQL, MySQL or SQLite database as
//! nodes and relationships.
//!
//! What is a node and what is a relationship comes from a mapping, written in
//! the source's `nodes` and `edges` or, when neither is given, inferred from
//! primary and foreign keys (see `khonsu_tables`). The rows are copied into
//! the graph: this is a sync, and the graph is as fresh as the last one.

mod lite;
mod my;
mod pg;
mod tls;

use khonsu_plugin_protocol::{
    read_request, ConfigField, Describe, EdgeRecord, Emitter, FieldKind, Message, NodeRecord, Permissions, Request, SyncMode,
};
use khonsu_tables::{infer, mapping_from_config, run, summary, Driver, Mapping, Options, Sink};
use serde_json::{Map, Value};
use std::io::Write;
use std::process::ExitCode;
use tls::SslMode;

const DEFAULT_ROW_LIMIT: u64 = 1_000_000;
const DEFAULT_INCREMENTAL_COLUMN: &str = "updated_at";

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
        name: "SQL".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "PostgreSQL, MySQL and SQLite tables as nodes and relationships. Without a mapping, a table with a primary key is a node, a foreign key is a relationship, and a table that only joins two others is a relationship between them.".into(),
        config: vec![
            ConfigField {
                secret: true,
                ..field(
                    "url",
                    "Connection URL",
                    FieldKind::String,
                    "postgres://user:password@host:5432/database or mysql://user:password@host:3306/database; add ?sslmode=require for TLS. Kept in the keychain",
                )
            },
            field("file", "SQLite file", FieldKind::File, "A SQLite database file, instead of a connection URL"),
            field("schema", "Schema", FieldKind::String, "PostgreSQL schema (default: public), or a MySQL database other than the URL's"),
            field(
                "tables",
                "Tables",
                FieldKind::String,
                "Only these tables, comma-separated, when the mapping is inferred; all of them when empty",
            ),
            field(
                "nodes",
                "Node tables (advanced)",
                FieldKind::Json,
                r#"[{"table" | "query", "label", "id", "columns", "where", "updated_at"}]; "id" is the key column, or a list of them. With nodes or edges given, nothing is inferred"#,
            ),
            field(
                "edges",
                "Edge tables (advanced)",
                FieldKind::Json,
                r#"[{"table" | "query", "type", "from": {"label", "column"}, "to": {"label", "column"}, "id", "columns", "where", "updated_at"}]; each end names a node table's label and the column here that holds its key"#,
            ),
            ConfigField {
                default: Some(Value::String(DEFAULT_INCREMENTAL_COLUMN.into())),
                ..field(
                    "incremental_column",
                    "Incremental column",
                    FieldKind::String,
                    "When the mapping is inferred: a column that only grows as rows change. Tables that have it are read from where the last sync stopped; empty turns this off",
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
            field(
                "sslmode",
                "TLS",
                FieldKind::String,
                "disable, prefer, require, verify-ca or verify-full; overrides ?sslmode= in the URL. require encrypts without checking the certificate",
            ),
            field("ca_file", "CA certificate", FieldKind::File, "A PEM file of the authority that signed the server's certificate; the certificate is then checked"),
        ],
        permissions: Permissions {
            network: vec!["The database server you configure".into()],
            read_files: true,
            notes: vec![
                "Sends only SELECT statements, in a read-only session; a read-only database user is enough".into(),
                "Reads a local file only for a SQLite database or a CA certificate you point it at".into(),
            ],
            ..Permissions::default()
        },
        labels: Vec::new(),
        relationship_types: Vec::new(),
        supports_incremental: true,
        analyses: Vec::new(),
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Postgres(String),
    MySql(String),
    Sqlite(String),
}

#[derive(Debug)]
struct Config {
    target: Target,
    /// `None` when neither the URL nor the configuration says.
    sslmode: Option<SslMode>,
    ca_file: Option<String>,
    schema: Option<String>,
    tables: Vec<String>,
    mapping: Option<Mapping>,
    incremental_column: Option<String>,
    row_limit: u64,
}

fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Takes `sslmode` out of a URL's parameters: the drivers each read TLS their
/// own way, and it is decided here for all of them.
fn take_sslmode(url: &str) -> Result<(String, Option<SslMode>), String> {
    let Some((base, query)) = url.split_once('?') else {
        return Ok((url.to_string(), None));
    };
    let mut mode = None;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if matches!(key.to_ascii_lowercase().as_str(), "sslmode" | "ssl-mode" | "ssl_mode") {
            mode = Some(SslMode::parse(value)?);
        } else {
            kept.push(pair);
        }
    }
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    Ok((url, mode))
}

fn parse_config(request: &Request) -> Result<Config, String> {
    let c = &request.config;
    let url = text(&request.secrets, "url").or_else(|| text(c, "url"));
    let file = text(c, "file");
    let (target, url_mode) = match (url, file) {
        (Some(_), Some(_)) => return Err("give a connection URL or a SQLite file, not both".into()),
        (None, None) => return Err("give a connection URL (postgres://… or mysql://…) or a SQLite file".into()),
        (None, Some(file)) => (Target::Sqlite(file), None),
        (Some(url), None) => {
            let scheme = url.split_once(':').map(|(scheme, _)| scheme.to_ascii_lowercase()).unwrap_or_default();
            match scheme.as_str() {
                "postgres" | "postgresql" => {
                    let (url, mode) = take_sslmode(&url)?;
                    (Target::Postgres(url), mode)
                }
                "mysql" | "mariadb" => {
                    let (url, mode) = take_sslmode(&url)?;
                    let rest = url.split_once(':').map(|(_, rest)| rest).unwrap_or_default();
                    (Target::MySql(format!("mysql:{rest}")), mode)
                }
                "sqlite" | "file" => {
                    let rest = url.split_once(':').map(|(_, rest)| rest).unwrap_or_default();
                    let path = rest.strip_prefix("//").unwrap_or(rest);
                    (Target::Sqlite(path.to_string()), None)
                }
                // Not the URL itself: it holds a password.
                _ => return Err("the connection URL must start with postgres://, mysql:// or sqlite:".into()),
            }
        }
    };
    let sslmode = match text(c, "sslmode") {
        Some(word) => Some(SslMode::parse(&word)?),
        None => url_mode,
    };
    let row_limit = match c.get("row_limit") {
        None | Some(Value::Null) => DEFAULT_ROW_LIMIT,
        Some(Value::String(s)) if s.trim().is_empty() => DEFAULT_ROW_LIMIT,
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or("row_limit must be a whole number, 0 for no limit")?,
    };
    let incremental_column = match c.get("incremental_column") {
        None | Some(Value::Null) => Some(DEFAULT_INCREMENTAL_COLUMN.to_string()),
        Some(_) => text(c, "incremental_column"),
    };
    Ok(Config {
        target,
        sslmode,
        ca_file: text(c, "ca_file"),
        schema: text(c, "schema"),
        tables: text(c, "tables")
            .map(|list| list.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect())
            .unwrap_or_default(),
        mapping: mapping_from_config(c)?,
        incremental_column,
        row_limit,
    })
}

/// Connects, and says what it connected to.
fn connect(config: &Config) -> Result<(Box<dyn Driver>, String), String> {
    let ca = config.ca_file.as_deref();
    let schema = config.schema.as_deref();
    Ok(match &config.target {
        Target::Postgres(url) => {
            let mut driver = pg::Pg::connect(url, config.sslmode.unwrap_or(SslMode::Prefer), ca, schema)?;
            let version = driver.version()?;
            (Box::new(driver), version)
        }
        Target::MySql(url) => {
            let mut driver = my::My::connect(url, config.sslmode.unwrap_or(SslMode::Disable), ca, schema)?;
            let version = driver.version()?;
            (Box::new(driver), version)
        }
        Target::Sqlite(path) => (Box::new(lite::Lite::open(path)?), format!("SQLite file {path}")),
    })
}

/// The mapping to read by: the one written out, or one inferred from the
/// keys of the tables that are there. With it, what inferring left out.
fn mapping_for(config: &Config, driver: &mut dyn Driver) -> Result<(Mapping, Vec<String>, bool), String> {
    if let Some(mapping) = &config.mapping {
        return Ok((mapping.clone(), Vec::new(), false));
    }
    let mut tables = driver.catalog().map_err(|e| format!("reading the catalog: {e}"))?;
    if !config.tables.is_empty() {
        for wanted in &config.tables {
            if !tables.iter().any(|t| t.name.eq_ignore_ascii_case(wanted)) {
                let known: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
                return Err(format!("no table {wanted:?}; tables: {}", known.join(", ")));
            }
        }
        tables.retain(|t| config.tables.iter().any(|wanted| t.name.eq_ignore_ascii_case(wanted)));
    }
    if tables.is_empty() {
        return Err("no tables to read: the schema is empty, or this user may not read it".into());
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
    let (mut driver, version) = connect(config)?;
    let (mapping, notes, inferred) = mapping_for(config, driver.as_mut())?;
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
    let (mut driver, _) = connect(config)?;
    let (mapping, notes, inferred) = mapping_for(config, driver.as_mut())?;
    let mut sink = Out(out);
    if inferred {
        // The mapping in the form `nodes` and `edges` take, to start from when
        // writing one by hand.
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
    let outcome = run(driver.as_mut(), &mapping, &options, &mut sink)?;
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
    use khonsu_plugin_protocol::parse_line;
    use serde_json::json;

    fn request(config: Value, secrets: Value) -> Request {
        Request {
            protocol: 1,
            config: config.as_object().unwrap().clone(),
            secrets: secrets.as_object().unwrap().clone(),
            ..Request::default()
        }
    }

    #[test]
    fn the_url_says_which_database_and_how_to_reach_it() {
        let parse = |url: &str| parse_config(&request(json!({}), json!({ "url": url })));
        let pg = parse("postgresql://u:p@db:5432/shop?sslmode=verify-full&connect_timeout=5").unwrap();
        assert_eq!(pg.target, Target::Postgres("postgresql://u:p@db:5432/shop?connect_timeout=5".into()));
        assert_eq!(pg.sslmode, Some(SslMode::VerifyFull));
        let my = parse("mariadb://u:p@db/shop?ssl-mode=REQUIRED").unwrap();
        assert_eq!(my.target, Target::MySql("mysql://u:p@db/shop".into()));
        assert_eq!(my.sslmode, Some(SslMode::Require));
        assert_eq!(parse("sqlite:///var/data/shop.db").unwrap().target, Target::Sqlite("/var/data/shop.db".into()));
        // A URL that can't be used is not repeated: it holds a password.
        let error = parse("oracle://u:secret@db/x").unwrap_err();
        assert!(!error.contains("secret"), "{error}");
        assert!(parse_config(&request(json!({}), json!({}))).is_err());
    }

    fn shop(dir: &std::path::Path) -> String {
        let path = dir.join("shop.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, updated_at TEXT);
             CREATE TABLE products (sku TEXT PRIMARY KEY, name TEXT, price REAL, photo BLOB);
             CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers, updated_at TEXT);
             CREATE TABLE order_items (order_id INTEGER REFERENCES orders(id), sku TEXT REFERENCES products(sku), qty INTEGER,
                                       PRIMARY KEY (order_id, sku));
             CREATE TABLE audit_log (at TEXT, what TEXT);
             INSERT INTO customers VALUES (1, 'Ada', '2026-11-01'), (2, 'Lin', '2026-11-02');
             INSERT INTO products VALUES ('L-1', 'Lamp', 19.5, x'00ff'), ('D-2', 'Desk', 240, NULL);
             INSERT INTO orders VALUES (10, 1, '2026-11-01'), (11, 2, '2026-11-02'), (12, NULL, '2026-11-02');
             INSERT INTO order_items VALUES (10, 'L-1', 2), (11, 'D-2', 1);
             INSERT INTO audit_log VALUES ('2026-11-01', 'started');",
        )
        .unwrap();
        path.display().to_string()
    }

    fn sync_messages(request: &Request) -> Vec<Message> {
        let config = parse_config(request).unwrap();
        let mut out = Emitter::new(Vec::new());
        sync(request, &config, &mut out).unwrap();
        String::from_utf8(out.into_inner())
            .unwrap()
            .lines()
            .filter_map(|line| parse_line(line).map(|m| m.unwrap()))
            .collect()
    }

    fn records(messages: &[Message]) -> (Vec<&NodeRecord>, Vec<&EdgeRecord>) {
        (
            messages.iter().filter_map(|m| if let Message::Node(n) = m { Some(n) } else { None }).collect(),
            messages.iter().filter_map(|m| if let Message::Edge(e) = m { Some(e) } else { None }).collect(),
        )
    }

    #[test]
    fn a_sqlite_file_is_mapped_from_its_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = shop(dir.path());
        let config = parse_config(&request(json!({ "file": file }), json!({}))).unwrap();
        let said = test(&config).unwrap();
        assert!(said.contains("3 node table(s): Customers, Orders, Products"), "{said}");
        assert!(said.contains("(:Orders)-[:CUSTOMER]->(:Customers)"), "{said}");
        assert!(said.contains("(:Orders)-[:ORDER_ITEMS]->(:Products)"), "{said}");
        assert!(said.contains("audit_log: no primary key"), "{said}");

        let messages = sync_messages(&request(json!({ "file": file }), json!({})));
        let (nodes, edges) = records(&messages);
        assert_eq!(nodes.len(), 7);
        let lamp = nodes.iter().find(|n| n.id == "Products:L-1").unwrap();
        assert_eq!(lamp.properties["price"], 19.5);
        // Bytes and nulls are left out rather than sent as something else.
        assert!(!lamp.properties.contains_key("photo"));
        // Two orders have a customer, one does not; two rows join an order to a product.
        assert_eq!(edges.iter().filter(|e| e.rel_type == "CUSTOMER").count(), 2);
        let item = edges.iter().find(|e| e.rel_type == "ORDER_ITEMS" && e.source == "Orders:10").unwrap();
        assert_eq!((item.target.as_str(), &item.properties["qty"]), ("Products:L-1", &json!(2)));
        assert!(matches!(messages.last(), Some(Message::Done { nodes: 7, edges: 4, complete: true, .. })));

        // The cursor it reported bounds the next incremental sync.
        let cursor = messages.iter().find_map(|m| if let Message::Cursor { value } = m { Some(value.clone()) } else { None }).unwrap();
        assert_eq!(cursor["tables"]["n:Customers"], "2026-11-02");
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch("INSERT INTO customers VALUES (3, 'Noor', '2026-11-03')")
            .unwrap();
        let mut again = request(json!({ "file": file, "tables": "customers" }), json!({}));
        again.mode = SyncMode::Incremental;
        again.cursor = Some(cursor);
        let messages = sync_messages(&again);
        let (nodes, _) = records(&messages);
        assert_eq!(nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["Customers:3"]);
    }

    #[test]
    fn a_written_mapping_reads_queries_and_marks_a_cut_table_partial() {
        let dir = tempfile::tempdir().unwrap();
        let file = shop(dir.path());
        let messages = sync_messages(&request(
            json!({
                "file": file,
                "row_limit": 1,
                "nodes": [
                    {"table": "customers", "label": "Customer", "id": "id", "columns": ["name"], "where": "id = 1"},
                    {"query": "SELECT sku, upper(name) AS title FROM products WHERE price > 100", "label": "Product", "id": "sku"}
                ],
                "edges": [
                    {"query": "SELECT o.customer_id, i.sku, i.qty FROM orders o JOIN order_items i ON i.order_id = o.id",
                     "type": "BOUGHT", "from": {"label": "Customer", "column": "customer_id"}, "to": {"label": "Product", "column": "sku"}}
                ]
            }),
            json!({}),
        ));
        let (nodes, edges) = records(&messages);
        assert_eq!(nodes[0].properties, json!({"name": "Ada"}).as_object().unwrap().clone());
        assert_eq!(nodes[1].properties["title"], "DESK");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].properties, json!({"qty": 2}).as_object().unwrap().clone());
        let Some(Message::Done { complete, gaps, .. }) = messages.last() else {
            panic!("no done");
        };
        assert!(!complete);
        assert!(gaps[0].contains("BOUGHT") && gaps[0].contains("more than 1 rows"), "{gaps:?}");
    }
}
