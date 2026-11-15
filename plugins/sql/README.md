# SQL plugin

Tables of a PostgreSQL, MySQL or SQLite database as nodes and relationships.

```sh
khonsu plugin install sql --yes
khonsu init graphs/shop
khonsu source add --db graphs/shop --plugin sql --name shop \
  -c url=postgres://reader:secret@db.example.com:5432/shop --test
khonsu plugin sync --source shop
khonsu query --db graphs/shop --cypher \
  "MATCH (o:Orders)-[:CUSTOMER]->(c:Customers) RETURN c.name, count(o) ORDER BY count(o) DESC LIMIT 5"
```

The rows are copied into the graph. The graph is as fresh as the last sync:
this is not a live view of the database.

## Connecting

| Database | `url` |
|---|---|
| PostgreSQL | `postgres://user:password@host:5432/database` |
| MySQL, MariaDB | `mysql://user:password@host:3306/database` |
| SQLite | `-c file=/path/to/data.db`, or `sqlite:///path/to/data.db` |

`url` is kept in the system keychain. `schema` picks a PostgreSQL schema other
than `public`, or a MySQL database other than the URL's.

The plugin sends only `SELECT`, in a session set read-only (a SQLite file is
opened read-only). A database user with read access is enough.

TLS follows `sslmode`, in the URL (`?sslmode=require`) or as its own field:

| `sslmode` | What happens |
|---|---|
| `disable` | No TLS |
| `prefer` | PostgreSQL's default: TLS when the server offers it, unverified. For MySQL this is the default and means no TLS |
| `require` | TLS or no connection. The certificate is **not** checked |
| `verify-ca`, `verify-full` | The certificate is checked against `ca_file`, or the public roots when there is none. For PostgreSQL both also check the host name |

Giving a `ca_file` checks the certificate whatever `sslmode` says.

## The mapping

Without `nodes` or `edges`, the mapping is inferred from keys:

- a table with a primary key is a node table, labelled after the table
  (`order_items` → `OrderItems`);
- each of its foreign keys is a relationship from the row to the row it points
  at, typed after the column (`customer_id` → `CUSTOMER`);
- a table that only joins two others (two foreign keys, and no key beyond them)
  is a relationship between them, typed after the table, with its other columns
  as properties;
- a table with no primary key is left out, and the connection test says so.

`--test` on `source add` prints what was inferred. A sync logs the same mapping
as JSON, in the form `nodes` and `edges` take, to start from when writing one
by hand. `tables` narrows inference to the tables named.

Written out:

```json
{
  "nodes": [
    {"table": "customers", "label": "Customer", "id": "id", "columns": ["name", "tier"]},
    {"query": "SELECT sku, name FROM products WHERE active", "label": "Product", "id": "sku"}
  ],
  "edges": [
    {"table": "order_items", "type": "BOUGHT",
     "from": {"label": "Customer", "column": "customer_id"},
     "to": {"label": "Product", "column": "sku"},
     "where": "qty > 0"}
  ]
}
```

| Key | Meaning |
|---|---|
| `table` or `query` | What to read: a table, or a `SELECT` |
| `label`, `type` | The node label, or the relationship type |
| `id` | The key column, or a list of them. On an edge it is optional: without it, there is one relationship per pair of nodes |
| `from`, `to` | On an edge: the label of a node table, and the column(s) here holding its key |
| `columns` | The properties to keep. All columns when left out |
| `where` | A condition on the rows, as SQL |
| `updated_at` | A column that only grows as rows change, for incremental syncs |

Pass them with `-c 'nodes=[…]' -c 'edges=[…]'`. With either given, nothing is
inferred.

Nulls and binary columns are left out. PostgreSQL arrays and JSON columns
arrive as lists and maps.

## Incremental syncs

```sh
khonsu plugin sync --source shop --incremental
```

A table with an `updated_at` column (or the one `incremental_column` names, for
an inferred mapping) is read from where the last sync stopped. Other tables are
read whole.

An incremental sync adds and updates. A row deleted from the database stays in
the graph until the next full sync, which removes what it no longer sees. A
foreign key pointed at a different row moves its relationship; one set to null
leaves the old relationship until a full sync.

## Limits

- `row_limit` (1,000,000 per table by default, 0 for none): a larger table is
  read up to there, in no particular order, and the sync is recorded as partial,
  which removes nothing.
- A relationship whose end was not read as a node is skipped and counted in
  `skipped_edges`.
