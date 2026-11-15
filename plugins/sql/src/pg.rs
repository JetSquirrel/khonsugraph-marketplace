//! PostgreSQL. Rows are turned into JSON by the server (`row_to_json`), so
//! every column type arrives as the server itself would print it.

use crate::tls::{self, SslMode};
use khonsu_tables::{Driver, ForeignKey, Row, Table};
use postgres::fallible_iterator::FallibleIterator;
use serde_json::Value;
use std::time::Duration;

pub struct Pg {
    client: postgres::Client,
    schema: String,
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// What the server said, rather than "db error".
fn message(error: postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => match db.hint() {
            Some(hint) => format!("{} ({hint})", db.message()),
            None => db.message().to_string(),
        },
        None => {
            let mut text = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(inner) = source {
                text.push_str(&format!(": {inner}"));
                source = inner.source();
            }
            text
        }
    }
}

impl Pg {
    pub fn connect(url: &str, mode: SslMode, ca_file: Option<&str>, schema: Option<&str>) -> Result<Self, String> {
        let mut config: postgres::Config = url.parse().map_err(|e: postgres::Error| format!("the connection URL can't be read: {}", message(e)))?;
        config.connect_timeout(Duration::from_secs(15));
        config.application_name("khonsugraph");
        let client = match mode {
            SslMode::Disable => {
                config.ssl_mode(postgres::config::SslMode::Disable);
                config.connect(postgres::NoTls)
            }
            other => {
                config.ssl_mode(if other == SslMode::Prefer {
                    postgres::config::SslMode::Prefer
                } else {
                    postgres::config::SslMode::Require
                });
                let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls::client_config(other, ca_file)?);
                config.connect(tls)
            }
        };
        let mut client = client.map_err(message)?;
        // Nothing this plugin sends writes; this makes the server refuse it if anything did.
        client.batch_execute("SET default_transaction_read_only = on").map_err(message)?;
        Ok(Self {
            client,
            schema: schema.map(str::to_string).unwrap_or_else(|| "public".into()),
        })
    }

    pub fn version(&mut self) -> Result<String, String> {
        let mut version = String::new();
        self.rows("SELECT current_database() AS db, current_setting('server_version') AS v", &mut |row| {
            let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
            version = format!("PostgreSQL {}, database {}", text("v"), text("db"));
            Ok(())
        })?;
        Ok(version)
    }
}

/// A key: (kind, table, columns, referenced table, referenced columns).
type Key = (String, String, Vec<String>, Option<String>, Vec<String>);

fn names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

impl Driver for Pg {
    fn catalog(&mut self) -> Result<Vec<Table>, String> {
        let schema = literal(&self.schema);
        let mut tables: Vec<Table> = Vec::new();
        self.rows(
            &format!(
                "SELECT c.relname AS name, \
                 (SELECT array_agg(a.attname ORDER BY a.attnum) FROM pg_attribute a \
                   WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped) AS columns \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {schema} AND c.relkind IN ('r', 'p', 'v', 'm') \
                   AND has_table_privilege(c.oid, 'SELECT') AND NOT c.relispartition \
                 ORDER BY c.relname"
            ),
            &mut |row| {
                tables.push(Table {
                    name: row.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    columns: names(row.get("columns")),
                    ..Table::default()
                });
                Ok(())
            },
        )?;
        let columns_of = |ids: &str, relation: &str| {
            format!(
                "(SELECT array_agg(a.attname ORDER BY k.ord) FROM unnest({ids}) WITH ORDINALITY AS k(attnum, ord) \
                  JOIN pg_attribute a ON a.attrelid = {relation} AND a.attnum = k.attnum)"
            )
        };
        let mut keys: Vec<Key> = Vec::new();
        self.rows(
            &format!(
                "SELECT con.contype::text AS kind, cl.relname AS name, {} AS columns, \
                 CASE WHEN fns.nspname = {schema} THEN fcl.relname END AS ref_table, {} AS ref_columns \
                 FROM pg_constraint con \
                 JOIN pg_class cl ON cl.oid = con.conrelid \
                 JOIN pg_namespace ns ON ns.oid = cl.relnamespace \
                 LEFT JOIN pg_class fcl ON fcl.oid = con.confrelid \
                 LEFT JOIN pg_namespace fns ON fns.oid = fcl.relnamespace \
                 WHERE ns.nspname = {schema} AND con.contype IN ('p', 'f') \
                 ORDER BY cl.relname, con.conname",
                columns_of("con.conkey", "con.conrelid"),
                columns_of("con.confkey", "con.confrelid"),
            ),
            &mut |row| {
                let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
                keys.push((
                    text("kind").unwrap_or_default(),
                    text("name").unwrap_or_default(),
                    names(row.get("columns")),
                    text("ref_table"),
                    names(row.get("ref_columns")),
                ));
                Ok(())
            },
        )?;
        for (kind, name, columns, ref_table, ref_columns) in keys {
            let Some(table) = tables.iter_mut().find(|t| t.name == name) else {
                continue;
            };
            match (kind.as_str(), ref_table) {
                ("p", _) => table.primary_key = columns,
                // A key into another schema points at nothing this source reads.
                ("f", Some(ref_table)) => table.foreign_keys.push(ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                }),
                _ => {}
            }
        }
        Ok(tables)
    }

    fn quote(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn table_ref(&self, table: &str) -> String {
        format!("{}.{}", self.quote(&self.schema), self.quote(table))
    }

    fn as_text(&self, expr: &str) -> String {
        format!("({expr})::text")
    }

    fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String> {
        let wrapped = format!("SELECT row_to_json(khonsu_r)::text FROM ({sql}) AS khonsu_r");
        let mut rows = self.client.query_raw(wrapped.as_str(), std::iter::empty::<String>()).map_err(message)?;
        while let Some(row) = rows.next().map_err(message)? {
            let text: &str = row.try_get(0).map_err(message)?;
            match serde_json::from_str::<Value>(text).map_err(|e| format!("a row the server sent is not JSON: {e}"))? {
                Value::Object(row) => on_row(row)?,
                _ => return Err("a row the server sent is not a JSON object".into()),
            }
        }
        Ok(())
    }
}
