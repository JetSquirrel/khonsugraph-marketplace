//! MySQL and MariaDB.

use crate::tls::SslMode;
use khonsu_tables::{Driver, ForeignKey, Row, Table};
use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder, SslOpts};
use serde_json::Value;
use std::time::Duration;

pub struct My {
    conn: Conn,
    database: String,
    /// Whether tables are written with the database in front.
    qualify: bool,
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

impl My {
    pub fn connect(url: &str, mode: SslMode, ca_file: Option<&str>, schema: Option<&str>) -> Result<Self, String> {
        let opts = Opts::from_url(url).map_err(|e| format!("the connection URL can't be read: {e}"))?;
        let mut builder = OptsBuilder::from_opts(opts.clone()).tcp_connect_timeout(Some(Duration::from_secs(15)));
        // MySQL has no "try it, then fall back": prefer reads as off, like its own clients without --ssl.
        let ssl = match mode {
            SslMode::Disable | SslMode::Prefer => None,
            SslMode::Require => Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
            SslMode::VerifyCa => Some(
                SslOpts::default()
                    .with_root_cert_path(ca_file.map(std::path::PathBuf::from))
                    .with_danger_skip_domain_validation(true),
            ),
            SslMode::VerifyFull => Some(SslOpts::default().with_root_cert_path(ca_file.map(std::path::PathBuf::from))),
        };
        builder = builder.ssl_opts(ssl);
        let mut conn = Conn::new(builder).map_err(|e| e.to_string())?;
        conn.query_drop("SET SESSION TRANSACTION READ ONLY").map_err(|e| e.to_string())?;
        let database = match schema.map(str::to_string).or_else(|| opts.get_db_name().map(str::to_string)) {
            Some(database) => database,
            None => return Err("the connection URL names no database: mysql://user:password@host:3306/database".into()),
        };
        Ok(Self {
            conn,
            database,
            qualify: schema.is_some(),
        })
    }

    pub fn version(&mut self) -> Result<String, String> {
        let version: Option<String> = self.conn.query_first("SELECT version()").map_err(|e| e.to_string())?;
        Ok(format!("MySQL {}, database {}", version.unwrap_or_default(), self.database))
    }
}

/// One column of a key: (table, constraint, column, referenced table, referenced column).
type KeyPart = (String, String, String, Option<String>, Option<String>);

/// One cell, by what its column says it is: the text protocol sends
/// everything as bytes.
fn cell(value: mysql::Value, kind: ColumnType) -> Value {
    use ColumnType::*;
    match value {
        mysql::Value::NULL => Value::Null,
        mysql::Value::Int(n) => Value::from(n),
        mysql::Value::UInt(n) => Value::from(n),
        mysql::Value::Float(n) => serde_json::Number::from_f64(f64::from(n)).map(Value::Number).unwrap_or(Value::Null),
        mysql::Value::Double(n) => serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null),
        mysql::Value::Bytes(bytes) => {
            // Not text: bytes have no place in a property.
            let Ok(text) = String::from_utf8(bytes) else {
                return Value::Null;
            };
            match kind {
                MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => text
                    .parse::<i64>()
                    .map(Value::from)
                    .or_else(|_| text.parse::<u64>().map(Value::from))
                    .unwrap_or(Value::String(text)),
                MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE | MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => text
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or(Value::String(text)),
                MYSQL_TYPE_JSON => serde_json::from_str(&text).unwrap_or(Value::String(text)),
                _ => Value::String(text),
            }
        }
        // Only the binary protocol sends these; kept readable in case it does.
        other => Value::String(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

impl Driver for My {
    fn catalog(&mut self) -> Result<Vec<Table>, String> {
        let database = literal(&self.database);
        let mut tables: Vec<Table> = Vec::new();
        self.rows(
            &format!(
                "SELECT table_name AS name, column_name AS col FROM information_schema.columns \
                 WHERE table_schema = {database} ORDER BY table_name, ordinal_position"
            ),
            &mut |row| {
                let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                let name = text("name");
                if tables.last().is_none_or(|t| t.name != name) {
                    tables.push(Table {
                        name,
                        ..Table::default()
                    });
                }
                tables.last_mut().expect("just pushed").columns.push(text("col"));
                Ok(())
            },
        )?;
        // In key order.
        let mut parts: Vec<KeyPart> = Vec::new();
        self.rows(
            &format!(
                "SELECT table_name AS name, constraint_name AS con, column_name AS col, \
                 CASE WHEN referenced_table_schema = {database} THEN referenced_table_name END AS ref_table, \
                 referenced_column_name AS ref_col, referenced_table_schema AS ref_schema \
                 FROM information_schema.key_column_usage \
                 WHERE table_schema = {database} AND (constraint_name = 'PRIMARY' OR referenced_table_name IS NOT NULL) \
                 ORDER BY table_name, constraint_name, ordinal_position"
            ),
            &mut |row| {
                let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
                // A key into another database points at nothing this source reads.
                if text("ref_schema").is_some() && text("ref_table").is_none() {
                    return Ok(());
                }
                parts.push((
                    text("name").unwrap_or_default(),
                    text("con").unwrap_or_default(),
                    text("col").unwrap_or_default(),
                    text("ref_table"),
                    text("ref_col"),
                ));
                Ok(())
            },
        )?;
        let mut last: Option<(String, String)> = None;
        for (name, constraint, column, ref_table, ref_column) in parts {
            let Some(table) = tables.iter_mut().find(|t| t.name == name) else {
                continue;
            };
            match ref_table {
                None => table.primary_key.push(column),
                Some(ref_table) => {
                    let this = (name.clone(), constraint.clone());
                    if last.as_ref() != Some(&this) {
                        table.foreign_keys.push(ForeignKey {
                            ref_table,
                            ..ForeignKey::default()
                        });
                    }
                    let key = table.foreign_keys.last_mut().expect("just pushed");
                    key.columns.push(column);
                    key.ref_columns.extend(ref_column);
                    last = Some(this);
                }
            }
        }
        Ok(tables)
    }

    fn quote(&self, name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }

    fn table_ref(&self, table: &str) -> String {
        if self.qualify {
            format!("{}.{}", self.quote(&self.database), self.quote(table))
        } else {
            self.quote(table)
        }
    }

    fn as_text(&self, expr: &str) -> String {
        format!("CAST({expr} AS CHAR)")
    }

    fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String> {
        let mut result = self.conn.query_iter(sql).map_err(|e| e.to_string())?;
        let mut failed = None;
        for row in result.by_ref() {
            let row = row.map_err(|e| e.to_string())?;
            let columns = row.columns();
            let mut out = Row::new();
            for (at, value) in row.unwrap().into_iter().enumerate() {
                let column = &columns[at];
                out.insert(column.name_str().into_owned(), cell(value, column.column_type()));
            }
            if let Err(error) = on_row(out) {
                failed = Some(error);
                break;
            }
        }
        // Dropping the result reads what is left of it, so the connection can be used again.
        drop(result);
        failed.map_or(Ok(()), Err)
    }
}
