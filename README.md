# SQLite Plugin

A [hyper-mcp](https://github.com/hyper-mcp-rs/hyper-mcp) WebAssembly plugin (v2)
for interacting with one or more SQLite databases over the Model Context
Protocol. It bundles SQLite (via [`rusqlite`](https://crates.io/crates/rusqlite)
compiled to `wasm32-wasip1`), so no external SQLite installation is required.

Connections are keyed by an **alias** and stay open across tool calls until you
explicitly `close` them.

## Tools

| Tool | Description |
|------|-------------|
| `connect` | Open a database and register it under an alias. Takes `alias`, `path`, optional `mode` (`read` \| `write`, default `read`), and optional `create` (default `false`). Fails if the alias already exists. When `create` is `true`, opens a brand-new database in write mode (creating any missing parent directories) and fails if a file already exists at `path`. |
| `attach` | Attach another database to an existing connection. Takes `alias`, `db_name` (the SQL `AS` name), `path`, and optional `mode` (default `read`). Fails if the alias does not exist. |
| `close` | Close a connection by `alias`. Idempotent — closing an unknown alias succeeds and reports `closed: false`. |
| `connections` | List all open connection aliases with their path and mode. |
| `tables` | List all tables and views for an `alias`, across the main and any attached databases. |
| `describe_table` | Return the schema of a table: columns (type, nullability, default, primary-key position), foreign keys, indexes, and the original `CREATE` statement. Takes `alias`, `table`, and optional `db_name` (default `main`). |
| `execute_sql` | Run a single SQL statement on an `alias`. The plugin inspects the prepared statement's column count to decide whether to **query** (returns `columns` + `rows`) or **execute** (returns `rows_affected`). |

### Open modes

- `read` — opens read-only; the database file must already exist.
- `write` — opens read-write, creating the file if it does not exist.

`attach` applies the same semantics to the attached database via a SQLite URI
filename (`mode=ro` / `mode=rwc`).

Setting `create: true` on `connect` ignores `mode` and always opens in write
mode. It creates any missing parent directories and refuses to overwrite an
existing file, so it is the safe way to provision a fresh database.

### `execute_sql` result shape

`execute_sql` always returns structured content with a `result_type`
discriminator:

- `result_type: "query"` → `columns` (array of names) and `rows` (array of rows,
  each an array of values aligned with `columns`). `rows_affected` is `0`.
- `result_type: "execute"` → `rows_affected` (integer). `columns`/`rows` are empty.

SQLite values map to JSON as follows: `NULL` → `null`, `INTEGER` → number,
`REAL` → number, `TEXT` → string, and `BLOB` → `{ "$base64": "<data>" }`.

> `execute_sql` runs a **single** statement. To run several statements, call the
> tool once per statement.

## Configuration

Because the plugin runs in a WASM sandbox, it can only reach database files that
are explicitly mounted via `allowed_paths`. Map the host directory containing
your databases into the plugin and use the in-sandbox path in `connect`/`attach`.

```json
{
  "plugins": {
    "sqlite": {
      "url": "oci://ghcr.io/hyper-mcp-rs/sqlite-plugin:latest",
      "runtime_config": {
        "allowed_paths": ["/host/path/to/data:/data"]
      }
    }
  }
}
```

With the mapping above, open a database with:

```json
{ "alias": "app", "path": "/data/app.db", "mode": "write" }
```

For local development, point the plugin at a freshly built `.wasm`:

```json
{
  "plugins": {
    "sqlite": {
      "url": "file:///path/to/target/wasm32-wasip1/release/plugin.wasm",
      "runtime_config": {
        "allowed_paths": ["/host/path/to/data:/data"]
      }
    }
  }
}
```

## Building

The plugin targets `wasm32-wasip1`. Because the bundled SQLite is compiled from
C, a C toolchain that can target WebAssembly is required — the
[WASI SDK](https://github.com/WebAssembly/wasi-sdk/releases).

```sh
# One-time setup
rustup target add wasm32-wasip1

# Install the WASI SDK and point the C cross-compiler at it
export WASI_SDK_PATH=/opt/wasi-sdk
export CC_wasm32_wasip1="$WASI_SDK_PATH/bin/clang"
export AR_wasm32_wasip1="$WASI_SDK_PATH/bin/llvm-ar"

# Build
cargo build --release --target wasm32-wasip1
# Result: target/wasm32-wasip1/release/plugin.wasm
```

CI (`.github/workflows/ci.yml`) and the release workflows install the WASI SDK
and set these variables automatically.

## License

Apache-2.0. See [LICENSE](LICENSE).
