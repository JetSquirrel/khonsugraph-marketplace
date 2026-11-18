# DuckLocal plugin

Parquet, CSV, JSON and Excel files, and DuckDB databases, as nodes and
relationships. [DuckLocal](https://github.com/JetSquirrel/ducklocal) does the
reading: this plugin runs `ducklocal query` with the SQL of a mapping, so
whatever DuckLocal reads, a graph can be made from, and a query worked out in
DuckLocal is a query the mapping takes.

It needs the `ducklocal` command on `PATH` (DuckLocal's title bar has a button
that puts it there), or its path in the `ducklocal` field.

```sh
khonsu plugin install ducklocal --yes
khonsu graph init graphs/shop

# A DuckDB database: mapped from its primary and foreign keys.
khonsu source add --db graphs/shop --plugin ducklocal --name shop -c database=shop.duckdb --test

# Data files: they have no keys, so say what is what.
khonsu source add --db graphs/shop --plugin ducklocal --name files -c folder=data \
  -c 'nodes=[{"table": "customers.parquet", "label": "Customer", "id": "id"},
             {"table": "orders/*.csv", "label": "Order", "id": "id"}]' \
  -c 'edges=[{"table": "orders/*.csv", "type": "PLACED_BY",
              "from": {"label": "Order", "column": "id"},
              "to": {"label": "Customer", "column": "customer_id"}, "columns": []}]' --test

khonsu plugin sync --source files
```

The mapping, incremental syncs and `row_limit` work as in the
[SQL plugin](../sql/README.md#the-mapping). What differs:

- **`table`** is a table of the database, or a file as DuckDB reads one:
  `'customers.parquet'`, `'logs/*.csv'`. Without a database every `table` is a
  file. Relative names are read from `folder`.
- **`query`** is DuckDB SQL, so it can join files, call `read_csv(…)` with
  options, or read a URL.
- **Nothing is inferred from files.** Only a DuckDB database has keys to infer
  from.
- Decimals arrive as numbers and dates and timestamps as ISO text. Binary
  columns are left out.

Rows are read 10,000 at a time. DuckLocal prints at most 2,000,000 cells at
once, so a table of more than 200 columns needs `columns` to keep fewer.
