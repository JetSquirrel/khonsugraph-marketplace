//! SQLite, opened read-only from a local file.

use khonsu_tables::{Driver, ForeignKey, Row, Table};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

pub struct Lite {
    conn: Connection,
}

impl Lite {
    pub fn open(path: &str) -> Result<Self, String> {
        if !std::path::Path::new(path).is_file() {
            return Err(format!("{path}: no such SQLite file"));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("{path}: {e}"))?;
        Ok(Self { conn })
    }

    fn strings(&mut self, sql: &str, column: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        self.rows(sql, &mut |row| {
            if let Some(Value::String(text)) = row.get(column) {
                out.push(text.clone());
            }
            Ok(())
        })?;
        Ok(out)
    }
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Driver for Lite {
    fn catalog(&mut self) -> Result<Vec<Table>, String> {
        let names = self.strings(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
            "name",
        )?;
        let mut tables = Vec::new();
        for name in names {
            let mut table = Table {
                name: name.clone(),
                ..Table::default()
            };
            let mut key: Vec<(i64, String)> = Vec::new();
            self.rows(&format!("SELECT name, pk FROM pragma_table_info({}) ORDER BY cid", literal(&name)), &mut |row| {
                let column = row.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let position = row.get("pk").and_then(Value::as_i64).unwrap_or(0);
                if position > 0 {
                    key.push((position, column.clone()));
                }
                table.columns.push(column);
                Ok(())
            })?;
            key.sort();
            table.primary_key = key.into_iter().map(|(_, column)| column).collect();
            let mut keys: Vec<(i64, ForeignKey)> = Vec::new();
            self.rows(
                &format!(
                    "SELECT id, \"table\" AS ref_table, \"from\" AS col, \"to\" AS ref_col FROM pragma_foreign_key_list({}) ORDER BY id, seq",
                    literal(&name)
                ),
                &mut |row| {
                    let id = row.get("id").and_then(Value::as_i64).unwrap_or(0);
                    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
                    if keys.last().is_none_or(|(last, _)| *last != id) {
                        keys.push((
                            id,
                            ForeignKey {
                                ref_table: text("ref_table").unwrap_or_default(),
                                ..ForeignKey::default()
                            },
                        ));
                    }
                    let key = &mut keys.last_mut().expect("just pushed").1;
                    key.columns.extend(text("col"));
                    // `REFERENCES parent` alone means the parent's primary key.
                    key.ref_columns.extend(text("ref_col"));
                    Ok(())
                },
            )?;
            table.foreign_keys = keys.into_iter().map(|(_, key)| key).collect();
            tables.push(table);
        }
        // Fill in the keys that named only their table.
        let primary: Vec<(String, Vec<String>)> = tables.iter().map(|t| (t.name.clone(), t.primary_key.clone())).collect();
        for table in &mut tables {
            for key in &mut table.foreign_keys {
                if key.ref_columns.is_empty() {
                    if let Some((_, columns)) = primary.iter().find(|(name, _)| name.eq_ignore_ascii_case(&key.ref_table)) {
                        key.ref_columns = columns.clone();
                    }
                }
                if let Some((name, _)) = primary.iter().find(|(name, _)| name.eq_ignore_ascii_case(&key.ref_table)) {
                    key.ref_table = name.clone();
                }
            }
        }
        Ok(tables)
    }

    fn quote(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn as_text(&self, expr: &str) -> String {
        format!("CAST({expr} AS TEXT)")
    }

    fn rows(&mut self, sql: &str, on_row: &mut dyn FnMut(Row) -> Result<(), String>) -> Result<(), String> {
        let mut statement = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let names: Vec<String> = statement.column_names().map(str::to_string).collect();
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut out = Row::new();
            for (at, name) in names.iter().enumerate() {
                let value = match row.get_ref(at).map_err(|e| e.to_string())? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::from(n),
                    ValueRef::Real(n) => serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null),
                    ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
                    // Bytes have no place in a property.
                    ValueRef::Blob(_) => Value::Null,
                };
                out.insert(name.clone(), value);
            }
            on_row(out)?;
        }
        Ok(())
    }
}
