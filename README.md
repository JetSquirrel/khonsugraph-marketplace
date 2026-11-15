# KhonsuGraph marketplace

The official plugins for [KhonsuGraph](https://github.com/JetSquirrel/KhonsuGraph)
that are installed by name rather than shipped with the app. This repository
holds their source. Their packages are built by CI and attached to releases.

```sh
khonsu plugin search
khonsu plugin install sql --yes
```

In the window: **Add source → More Plugins…**. KhonsuGraph looks here by
default; to add it by hand, `khonsu marketplace add JetSquirrel/khonsugraph-marketplace`.

| Plugin | What |
|---|---|
| [`sql`](plugins/sql/README.md) | PostgreSQL, MySQL and SQLite tables as nodes and relationships, mapped by hand or from primary and foreign keys |
| [`ducklocal`](plugins/ducklocal/README.md) | Parquet, CSV, JSON and DuckDB tables as nodes and relationships, read through DuckLocal |

A plugin is a program that runs as you. Installing one from here checks that
it is the package listed, and it still needs your approval before it runs.

## Layout

| Path | What |
|---|---|
| `plugins/<id>/` | One plugin: `khonsu-plugin.json`, its source, its README |
| `crates/khonsu-tables` | The table-to-graph mapping the SQL and DuckLocal plugins share |
| `marketplace.json` | The index KhonsuGraph reads: each plugin's version, permissions and packages with their SHA-256. Written by CI; don't edit it by hand |
| `scripts/marketplace.py` | What CI runs: which versions to release, packing, listing |

## Releasing a plugin

**Raise its version** in `khonsu-plugin.json` (and its `Cargo.toml`), and merge
to `main`. That is all.

On each push to `main`, CI looks for plugins whose version has no release tag
(`<id>-v<version>`). Only those are built, for macOS (arm64) and Windows
(x86_64), packed, and attached to a release of that tag. `marketplace.json` is
then updated with the new packages and their hashes, and committed. A plugin
whose version did not change is not built, and its entry is left as it is.

A pull request that changes a plugin's files without raising its version fails
CI: the change would never be released. A change to `crates/` alone releases
nothing; raise the version of each plugin that should pick it up.

A released version is not rebuilt. To fix one, release the next version.

## Adding a plugin

Add `plugins/<id>/` with a `khonsu-plugin.json` whose `id` is the folder's name.

- **Compiled** (it has a `Cargo.toml`): add it to the workspace in `Cargo.toml`,
  name the package `khonsu-plugin-<id>`, and make `entry` that name. The package
  is the manifest, the executable, and `templates/` if there is one.
- **A script** (`"entry": ["python3", "plugin.py"]`): one package for every
  platform, holding the folder without `tests/`, `README.md` and hidden files.

See [writing a plugin](https://github.com/JetSquirrel/KhonsuGraph/blob/main/docs/plugins.md).

## Building here

```sh
cargo test --workspace
python3 scripts/marketplace.py check        # manifests are usable
python3 scripts/marketplace.py plan         # what the next push would release
cargo build --release -p khonsu-plugin-sql
python3 scripts/marketplace.py pack sql --target macos-aarch64 --bin target/release/khonsu-plugin-sql
khonsu plugin install dist/sql-0.3.0-macos-aarch64.zip    # try the package
```

## Where packages are served

From this repository's releases. The index holds each package's full address,
so they can move: set the repository variable `PACKAGE_BASE_URL` (for example
`https://marketplace.khonsugraph.com/{tag}`, where `{tag}` is `<id>-v<version>`),
add a step to `release.yml` that uploads `dist/*.zip` there, and the next
releases are listed at the new address. Packages already listed keep theirs.
