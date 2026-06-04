mod pdk;
mod types;

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Result, anyhow};
use base64::Engine;
use pdk::types::*;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use schemars::schema_for;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::types::*;

/// An open connection plus the metadata we report back to callers.
struct ConnEntry {
    conn: Connection,
    path: String,
    mode: AccessMode,
}

/// Registry of open connections, keyed by alias.
///
/// WASM is single-threaded and the plugin instance's linear memory (including
/// this global) persists across host calls, so connections stay open until they
/// are explicitly closed. `Connection` is `Send` but `!Sync`; wrapping it in a
/// `Mutex` makes the static `Sync`. `BTreeMap::new()` and `Mutex::new()` are
/// both `const`, so no lazy initialization is required (and aliases list in
/// sorted order for free).
static CONNECTIONS: Mutex<BTreeMap<String, ConnEntry>> = Mutex::new(BTreeMap::new());

fn registry() -> MutexGuard<'static, BTreeMap<String, ConnEntry>> {
    // Recover from a poisoned lock rather than cascading panics: the data is
    // still structurally valid, a previous call simply panicked while holding it.
    CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn call_tool(input: CallToolRequest) -> Result<CallToolResult> {
    Ok(match input.request.name.as_str() {
        "connect" => connect(input),
        "attach" => attach(input),
        "close" => close(input),
        "connections" => connections(input),
        "tables" => tables(input),
        "describe_table" => describe_table(input),
        "execute_sql" => execute_sql(input),
        other => CallToolResult::error(format!("Unknown tool: {other}")),
    })
}

// --- shared helpers ------------------------------------------------------

/// Deserialize the tool arguments into `T`, returning an error result on failure.
fn parse_args<T: DeserializeOwned>(input: &CallToolRequest) -> Result<T, CallToolResult> {
    let map = input.request.arguments.clone().unwrap_or_default();
    serde_json::from_value(Value::Object(map))
        .map_err(|e| CallToolResult::error(format!("Invalid arguments: {e}")))
}

/// Build a successful `CallToolResult` carrying `value` as both structured
/// content and a pretty-printed JSON text block.
fn ok_result<T: Serialize>(value: &T) -> CallToolResult {
    let structured = match serde_json::to_value(value) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    };
    let text = structured
        .as_ref()
        .and_then(|m| serde_json::to_string_pretty(m).ok())
        .unwrap_or_default();
    CallToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text,
            ..Default::default()
        })],
        structured_content: structured,
        ..Default::default()
    }
}

/// Quote an SQL identifier (double quotes, doubling embedded quotes) so that
/// schema/table/index names can be interpolated safely.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn open_flags(mode: AccessMode) -> OpenFlags {
    let base = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match mode {
        AccessMode::Read => base | OpenFlags::SQLITE_OPEN_READ_ONLY,
        AccessMode::Write => {
            base | OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        }
    }
}

// --- connect -------------------------------------------------------------

fn connect(input: CallToolRequest) -> CallToolResult {
    let args: ConnectArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let mut conns = registry();
    if conns.contains_key(&args.alias) {
        return CallToolResult::error(format!("Connection alias '{}' already exists", args.alias));
    }

    // `create` always opens a fresh database in write mode: refuse to clobber an
    // existing file and create any missing parent directories first.
    let mode = if args.create {
        let path = std::path::Path::new(&args.path);
        if path.exists() {
            return CallToolResult::error(format!(
                "Cannot create '{}': a file already exists at that path",
                args.path
            ));
        }
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return CallToolResult::error(format!(
                        "Failed to create directories for '{}': {e}",
                        args.path
                    ));
                }
            }
            _ => {}
        }
        AccessMode::Write
    } else {
        args.mode.unwrap_or_default()
    };

    let conn = match Connection::open_with_flags(&args.path, open_flags(mode)) {
        Ok(c) => c,
        Err(e) => {
            return CallToolResult::error(format!("Failed to open '{}': {e}", args.path));
        }
    };

    conns.insert(
        args.alias.clone(),
        ConnEntry {
            conn,
            path: args.path.clone(),
            mode,
        },
    );

    ok_result(&ConnectResponse {
        message: format!(
            "Opened '{}' as connection '{}' ({} mode)",
            args.path,
            args.alias,
            mode.as_str()
        ),
        alias: args.alias,
        path: args.path,
        mode,
    })
}

// --- attach --------------------------------------------------------------

fn attach(input: CallToolRequest) -> CallToolResult {
    let args: AttachArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let mode = args.mode.unwrap_or_default();

    let mut conns = registry();
    let entry = match conns.get_mut(&args.alias) {
        Some(e) => e,
        None => {
            return CallToolResult::error(format!(
                "Connection alias '{}' does not exist",
                args.alias
            ));
        }
    };

    // Use a URI filename so the attached database honors the requested mode.
    // The connection was opened with SQLITE_OPEN_URI, so this is interpreted as
    // a URI. The schema name in `AS` must be an identifier, so it is quoted
    // rather than bound.
    let uri = format!("file:{}?mode={}", args.path, mode.uri_mode());
    let sql = format!("ATTACH DATABASE ?1 AS {}", quote_ident(&args.db_name));
    if let Err(e) = entry.conn.execute(&sql, rusqlite::params![uri]) {
        return CallToolResult::error(format!(
            "Failed to attach '{}' as '{}': {e}",
            args.path, args.db_name
        ));
    }

    ok_result(&AttachResponse {
        message: format!(
            "Attached '{}' as '{}' on connection '{}' ({} mode)",
            args.path,
            args.db_name,
            args.alias,
            mode.as_str()
        ),
        alias: args.alias,
        db_name: args.db_name,
        path: args.path,
        mode,
    })
}

// --- close ---------------------------------------------------------------

fn close(input: CallToolRequest) -> CallToolResult {
    let args: CloseArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };

    // Dropping the entry closes the underlying connection. Removal is
    // idempotent: closing an alias that is not open is a no-op.
    registry().remove(&args.alias);

    CallToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text: "Closed".to_string(),
            ..Default::default()
        })],
        ..Default::default()
    }
}

// --- connections ---------------------------------------------------------

fn connections(_input: CallToolRequest) -> CallToolResult {
    let conns = registry();
    let list = conns
        .iter()
        .map(|(alias, entry)| ConnectionInfo {
            alias: alias.clone(),
            path: entry.path.clone(),
            mode: entry.mode,
        })
        .collect();
    ok_result(&ConnectionsResponse { connections: list })
}

// --- tables --------------------------------------------------------------

fn tables(input: CallToolRequest) -> CallToolResult {
    let args: TablesArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let conns = registry();
    let entry = match conns.get(&args.alias) {
        Some(e) => e,
        None => {
            return CallToolResult::error(format!(
                "Connection alias '{}' does not exist",
                args.alias
            ));
        }
    };

    match list_tables(&entry.conn) {
        Ok(tables) => ok_result(&TablesResponse {
            alias: args.alias,
            tables,
        }),
        Err(e) => CallToolResult::error(format!("Failed to list tables: {e}")),
    }
}

fn list_tables(conn: &Connection) -> rusqlite::Result<Vec<TableEntry>> {
    // Enumerate every attached schema (main, temp, and ATTACHed databases).
    let schemas: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA database_list")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = Vec::new();
    for schema in schemas {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
            quote_ident(&schema)
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(TableEntry {
                schema: schema.clone(),
                name: row.get(0)?,
                r#type: row.get(1)?,
            })
        })?;
        for entry in rows {
            out.push(entry?);
        }
    }
    Ok(out)
}

// --- describe_table ------------------------------------------------------

fn describe_table(input: CallToolRequest) -> CallToolResult {
    let args: DescribeTableArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let schema = args.db_name.unwrap_or_else(|| "main".to_string());

    let conns = registry();
    let entry = match conns.get(&args.alias) {
        Some(e) => e,
        None => {
            return CallToolResult::error(format!(
                "Connection alias '{}' does not exist",
                args.alias
            ));
        }
    };

    match describe(&entry.conn, &schema, &args.table) {
        Ok(Some(mut resp)) => {
            resp.alias = args.alias;
            ok_result(&resp)
        }
        Ok(None) => CallToolResult::error(format!(
            "Table '{}' not found in schema '{}'",
            args.table, schema
        )),
        Err(e) => CallToolResult::error(format!("Failed to describe table: {e}")),
    }
}

/// Returns `Ok(None)` if the table has no columns (i.e. does not exist).
fn describe(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> rusqlite::Result<Option<DescribeTableResponse>> {
    let columns = table_columns(conn, schema, table)?;
    if columns.is_empty() {
        return Ok(None);
    }

    Ok(Some(DescribeTableResponse {
        alias: String::new(), // filled in by the caller
        schema: schema.to_string(),
        table: table.to_string(),
        columns,
        foreign_keys: table_foreign_keys(conn, schema, table)?,
        indexes: table_indexes(conn, schema, table)?,
        create_sql: table_create_sql(conn, schema, table)?,
    }))
}

fn table_columns(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> rusqlite::Result<Vec<ColumnInfo>> {
    // PRAGMA table_info: cid, name, type, notnull, dflt_value, pk
    let sql = format!(
        "PRAGMA {}.table_info({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(ColumnInfo {
            cid: row.get(0)?,
            name: row.get(1)?,
            r#type: row.get(2)?,
            not_null: row.get::<_, i64>(3)? != 0,
            default_value: row.get::<_, Option<String>>(4)?,
            primary_key: row.get(5)?,
        })
    })?;
    rows.collect()
}

fn table_foreign_keys(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> rusqlite::Result<Vec<ForeignKeyInfo>> {
    // PRAGMA foreign_key_list: id, seq, table, from, to, on_update, on_delete, match
    let sql = format!(
        "PRAGMA {}.foreign_key_list({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(ForeignKeyInfo {
            id: row.get(0)?,
            seq: row.get(1)?,
            table: row.get(2)?,
            from: row.get(3)?,
            to: row.get::<_, Option<String>>(4)?,
            on_update: row.get(5)?,
            on_delete: row.get(6)?,
            r#match: row.get(7)?,
        })
    })?;
    rows.collect()
}

fn table_indexes(conn: &Connection, schema: &str, table: &str) -> rusqlite::Result<Vec<IndexInfo>> {
    // PRAGMA index_list: seq, name, unique, origin, partial
    let list_sql = format!(
        "PRAGMA {}.index_list({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let base: Vec<(String, bool, String, bool)> = {
        let mut stmt = conn.prepare(&list_sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = Vec::with_capacity(base.len());
    for (name, unique, origin, partial) in base {
        // PRAGMA index_info: seqno, cid, name (name is NULL for expression columns)
        let info_sql = format!(
            "PRAGMA {}.index_info({})",
            quote_ident(schema),
            quote_ident(&name)
        );
        let mut stmt = conn.prepare(&info_sql)?;
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .map(|c| c.map(|opt| opt.unwrap_or_else(|| "<expr>".to_string())))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push(IndexInfo {
            name,
            unique,
            origin,
            partial,
            columns,
        });
    }
    Ok(out)
}

fn table_create_sql(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> rusqlite::Result<Option<String>> {
    let sql = format!(
        "SELECT sql FROM {}.sqlite_master \
         WHERE type IN ('table', 'view') AND name = ?1",
        quote_ident(schema)
    );
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_row(rusqlite::params![table], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

// --- execute_sql ---------------------------------------------------------

fn execute_sql(input: CallToolRequest) -> CallToolResult {
    let args: ExecuteSqlArguments = match parse_args(&input) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let conns = registry();
    let entry = match conns.get(&args.alias) {
        Some(e) => e,
        None => {
            return CallToolResult::error(format!(
                "Connection alias '{}' does not exist",
                args.alias
            ));
        }
    };

    match run_sql(&entry.conn, &args.sql) {
        Ok(resp) => ok_result(&resp),
        Err(e) => CallToolResult::error(format!("SQL error: {e}")),
    }
}

fn run_sql(conn: &Connection, sql: &str) -> rusqlite::Result<ExecuteSqlResponse> {
    let mut stmt = conn.prepare(sql)?;
    let col_count = stmt.column_count();

    // A prepared statement that produces columns is a query; otherwise it is a
    // mutation/DDL statement and we report the affected row count.
    if col_count == 0 {
        let affected = stmt.execute([])?;
        return Ok(ExecuteSqlResponse::Execute(ExecuteResult {
            rows_affected: affected as i64,
        }));
    }

    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();

    let mut rows_out: Vec<Vec<Value>> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut record = Vec::with_capacity(col_count);
        for i in 0..col_count {
            record.push(sqlite_to_json(row.get::<_, SqlValue>(i)?));
        }
        rows_out.push(record);
    }

    Ok(ExecuteSqlResponse::Query(QueryResult {
        columns,
        rows: rows_out,
    }))
}

fn sqlite_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(i),
        // `Value::from(f64)` maps non-finite values to Null, so this never panics.
        SqlValue::Real(f) => Value::from(f),
        SqlValue::Text(s) => Value::from(s),
        SqlValue::Blob(b) => {
            json!({ "$base64": base64::engine::general_purpose::STANDARD.encode(b) })
        }
    }
}

// --- tool listing --------------------------------------------------------

pub(crate) fn list_tools(_input: ListToolsRequest) -> Result<ListToolsResult> {
    Ok(ListToolsResult {
        tools: vec![
            Tool {
                name: "connect".to_string(),
                title: Some("Open SQLite connection".to_string()),
                description: Some(
                    "Open a SQLite database and register it under an alias. Fails if the alias \
                     already exists. Set `create` to true to create a new database (in write \
                     mode, making parent directories as needed) that fails if one already \
                     exists at the path. The connection stays open until `close` is called."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    ..Default::default()
                }),
                input_schema: schema_for!(ConnectArguments),
                output_schema: Some(schema_for!(ConnectResponse)),
            },
            Tool {
                name: "attach".to_string(),
                title: Some("Attach database".to_string()),
                description: Some(
                    "Attach another SQLite database to an existing connection under a schema name \
                     (the SQL `AS` clause). Fails if the alias does not exist."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    ..Default::default()
                }),
                input_schema: schema_for!(AttachArguments),
                output_schema: Some(schema_for!(AttachResponse)),
            },
            Tool {
                name: "close".to_string(),
                title: Some("Close connection".to_string()),
                description: Some(
                    "Close a connection by alias. Idempotent: closing an alias that is not open \
                     still succeeds."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(true),
                    ..Default::default()
                }),
                input_schema: schema_for!(CloseArguments),
                output_schema: None,
            },
            Tool {
                name: "connections".to_string(),
                title: Some("List connections".to_string()),
                description: Some(
                    "List all open connection aliases along with their path and open mode."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(true),
                    ..Default::default()
                }),
                input_schema: schema_for!(ConnectionsArguments),
                output_schema: Some(schema_for!(ConnectionsResponse)),
            },
            Tool {
                name: "tables".to_string(),
                title: Some("List tables".to_string()),
                description: Some(
                    "List all tables and views for a connection, across the main database and any \
                     attached databases."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(true),
                    ..Default::default()
                }),
                input_schema: schema_for!(TablesArguments),
                output_schema: Some(schema_for!(TablesResponse)),
            },
            Tool {
                name: "describe_table".to_string(),
                title: Some("Describe table".to_string()),
                description: Some(
                    "Return the schema of a table: columns (with type, nullability, default and \
                     primary-key position), foreign keys, indexes, and the original CREATE \
                     statement. Use `db_name` to target an attached database (default 'main')."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(true),
                    ..Default::default()
                }),
                input_schema: schema_for!(DescribeTableArguments),
                output_schema: Some(schema_for!(DescribeTableResponse)),
            },
            Tool {
                name: "execute_sql".to_string(),
                title: Some("Execute SQL".to_string()),
                description: Some(
                    "Run a single SQL statement on a connection. The plugin decides automatically \
                     (from the prepared statement's column count) whether to query (returning \
                     columns and rows) or execute (returning the number of affected rows)."
                        .to_string(),
                ),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    ..Default::default()
                }),
                input_schema: schema_for!(ExecuteSqlArguments),
                output_schema: Some(schema_for!(ExecuteSqlResponse)),
            },
        ],
    })
}

// --- unused MCP handlers (defaults) --------------------------------------

// Provide completion suggestions for a partially-typed input.
pub(crate) fn complete(_input: CompleteRequest) -> Result<CompleteResult> {
    Ok(CompleteResult::default())
}

// Retrieve a specific prompt by name.
pub(crate) fn get_prompt(_input: GetPromptRequest) -> Result<GetPromptResult> {
    Err(anyhow!("Prompts are not supported by this plugin"))
}

// List all available prompts.
pub(crate) fn list_prompts(_input: ListPromptsRequest) -> Result<ListPromptsResult> {
    Ok(ListPromptsResult::default())
}

// List all available resource templates.
pub(crate) fn list_resource_templates(
    _input: ListResourceTemplatesRequest,
) -> Result<ListResourceTemplatesResult> {
    Ok(ListResourceTemplatesResult::default())
}

// List all available resources.
pub(crate) fn list_resources(_input: ListResourcesRequest) -> Result<ListResourcesResult> {
    Ok(ListResourcesResult::default())
}

// Notification that the list of roots has changed.
pub(crate) fn on_roots_list_changed(_input: PluginNotificationContext) -> Result<()> {
    Ok(())
}

// Read the contents of a resource by its URI.
pub(crate) fn read_resource(_input: ReadResourceRequest) -> Result<ReadResourceResult> {
    Err(anyhow!("Resources are not supported by this plugin"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- test helpers ----------------------------------------------------

    /// Build a `CallToolRequest` for `name` with the given JSON arguments.
    /// A JSON object becomes the arguments map; `Value::Null` means no arguments.
    fn req(name: &str, args: Value) -> CallToolRequest {
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => panic!("arguments must be a JSON object or null, got {other}"),
        };
        CallToolRequest {
            request: CallToolRequestParam {
                name: name.to_string(),
                arguments,
            },
            ..Default::default()
        }
    }

    /// Deserialize the structured content of a successful result into `T`.
    fn structured<T: DeserializeOwned>(res: &CallToolResult) -> T {
        let map = res
            .structured_content
            .clone()
            .expect("successful result should carry structured content");
        serde_json::from_value(Value::Object(map)).expect("structured content should deserialize")
    }

    /// Extract the text content block of a result.
    fn text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text(t)) => t.text.clone(),
            other => panic!("expected a text content block, got {other:?}"),
        }
    }

    fn is_ok(res: &CallToolResult) -> bool {
        res.is_error != Some(true)
    }

    /// Open an in-memory database and register it under `alias` (write mode).
    /// Returns after the connection is in the registry.
    fn open_memory(alias: &str) {
        let res = connect(req(
            "connect",
            json!({ "alias": alias, "path": ":memory:", "mode": "write" }),
        ));
        assert!(is_ok(&res), "connect failed: {}", text(&res));
    }

    fn drop_conn(alias: &str) {
        registry().remove(alias);
    }

    // --- quote_ident -----------------------------------------------------

    #[test]
    fn quote_ident_wraps_in_double_quotes() {
        assert_eq!(quote_ident("main"), "\"main\"");
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("\""), "\"\"\"\"");
    }

    // --- open_flags ------------------------------------------------------

    #[test]
    fn open_flags_read_is_read_only() {
        let flags = open_flags(AccessMode::Read);
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY));
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_URI));
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_NO_MUTEX));
        assert!(!flags.contains(OpenFlags::SQLITE_OPEN_CREATE));
        assert!(!flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE));
    }

    #[test]
    fn open_flags_write_creates_and_is_read_write() {
        let flags = open_flags(AccessMode::Write);
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE));
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_CREATE));
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_URI));
        assert!(flags.contains(OpenFlags::SQLITE_OPEN_NO_MUTEX));
        assert!(!flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY));
    }

    // --- sqlite_to_json --------------------------------------------------

    #[test]
    fn sqlite_to_json_scalars() {
        assert_eq!(sqlite_to_json(SqlValue::Null), Value::Null);
        assert_eq!(sqlite_to_json(SqlValue::Integer(42)), json!(42));
        assert_eq!(sqlite_to_json(SqlValue::Real(1.5)), json!(1.5));
        assert_eq!(sqlite_to_json(SqlValue::Text("hi".into())), json!("hi"));
    }

    #[test]
    fn sqlite_to_json_blob_is_base64_object() {
        // base64(STANDARD) of [0x01, 0xff] == "Af8="
        assert_eq!(
            sqlite_to_json(SqlValue::Blob(vec![0x01, 0xff])),
            json!({ "$base64": "Af8=" })
        );
    }

    // --- parse_args ------------------------------------------------------

    #[test]
    fn parse_args_valid() {
        let input = req(
            "connect",
            json!({ "alias": "a", "path": "p", "mode": "write" }),
        );
        let args: ConnectArguments = parse_args(&input).unwrap();
        assert_eq!(args.alias, "a");
        assert_eq!(args.mode, Some(AccessMode::Write));
    }

    #[test]
    fn parse_args_missing_arguments_uses_empty_object() {
        // ConnectionsArguments has no required fields, so absent arguments parse fine.
        let input = req("connections", Value::Null);
        let args: Result<ConnectionsArguments, _> = parse_args(&input);
        assert!(args.is_ok());
    }

    #[test]
    fn parse_args_invalid_returns_error_result() {
        let input = req("connect", json!({ "path": "p" })); // missing alias
        let err = parse_args::<ConnectArguments>(&input).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        assert!(text(&err).contains("Invalid arguments"));
    }

    // --- ok_result -------------------------------------------------------

    #[test]
    fn ok_result_carries_structured_and_text() {
        let resp = ConnectResponse {
            alias: "a".into(),
            path: "/tmp/db".into(),
            mode: AccessMode::Write,
            message: "opened".into(),
        };
        let res = ok_result(&resp);
        assert!(res.is_error.is_none());

        let parsed: ConnectResponse = structured(&res);
        assert_eq!(parsed.alias, "a");
        assert_eq!(parsed.mode, AccessMode::Write);

        // The text block is the pretty-printed JSON of the same payload.
        let from_text: ConnectResponse = serde_json::from_str(&text(&res)).unwrap();
        assert_eq!(from_text.message, "opened");
    }

    // --- run_sql ---------------------------------------------------------

    fn as_query(res: ExecuteSqlResponse) -> QueryResult {
        match res {
            ExecuteSqlResponse::Query(q) => q,
            other => panic!("expected a query result, got {other:?}"),
        }
    }

    fn as_execute(res: ExecuteSqlResponse) -> ExecuteResult {
        match res {
            ExecuteSqlResponse::Execute(e) => e,
            other => panic!("expected an execute result, got {other:?}"),
        }
    }

    #[test]
    fn run_sql_execute_reports_affected_rows() {
        let conn = Connection::open_in_memory().unwrap();

        let create = as_execute(run_sql(&conn, "CREATE TABLE t (a INTEGER, b TEXT)").unwrap());
        assert_eq!(create.rows_affected, 0);

        let insert = as_execute(
            run_sql(
                &conn,
                "INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y'), (3, 'z')",
            )
            .unwrap(),
        );
        assert_eq!(insert.rows_affected, 3);
    }

    #[test]
    fn run_sql_query_returns_columns_and_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a INTEGER, b TEXT);\
             INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL);",
        )
        .unwrap();

        let res = as_query(run_sql(&conn, "SELECT a, b FROM t ORDER BY a").unwrap());
        assert_eq!(res.columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0], vec![json!(1), json!("x")]);
        assert_eq!(res.rows[1], vec![json!(2), Value::Null]);
    }

    #[test]
    fn run_sql_query_encodes_blob_as_base64() {
        let conn = Connection::open_in_memory().unwrap();
        let res = as_query(run_sql(&conn, "SELECT x'01ff' AS data").unwrap());
        assert_eq!(res.columns, vec!["data".to_string()]);
        assert_eq!(res.rows[0][0], json!({ "$base64": "Af8=" }));
    }

    #[test]
    fn run_sql_propagates_errors() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(run_sql(&conn, "SELECT * FROM does_not_exist").is_err());
    }

    // --- list_tables -----------------------------------------------------

    #[test]
    fn list_tables_includes_tables_and_views_excluding_internal() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE beta (x);\
             CREATE TABLE alpha (y);\
             CREATE VIEW v_alpha AS SELECT y FROM alpha;",
        )
        .unwrap();

        let tables = list_tables(&conn).unwrap();

        // All in the main schema; sqlite_* internal objects excluded.
        assert!(tables.iter().all(|t| t.schema == "main"));
        assert!(tables.iter().all(|t| !t.name.starts_with("sqlite_")));

        let names: Vec<(&str, &str)> = tables
            .iter()
            .map(|t| (t.name.as_str(), t.r#type.as_str()))
            .collect();
        assert!(names.contains(&("alpha", "table")));
        assert!(names.contains(&("beta", "table")));
        assert!(names.contains(&("v_alpha", "view")));

        // ORDER BY type, name: tables (sorted) come before views.
        let types: Vec<&str> = tables.iter().map(|t| t.r#type.as_str()).collect();
        let first_view = types.iter().position(|&t| t == "view").unwrap();
        assert!(types[..first_view].iter().all(|&t| t == "table"));
    }

    // --- describe and its sub-queries -----------------------------------

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT);\
             CREATE TABLE child (\
                 id INTEGER PRIMARY KEY,\
                 parent_id INTEGER NOT NULL,\
                 note TEXT DEFAULT 'hi',\
                 FOREIGN KEY (parent_id) REFERENCES parent (id)\
             );\
             CREATE INDEX idx_child_note ON child (note);\
             CREATE UNIQUE INDEX idx_child_unique ON child (parent_id, note);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn table_columns_reports_metadata() {
        let conn = schema_conn();
        let cols = table_columns(&conn, "main", "child").unwrap();

        let by_name = |name: &str| cols.iter().find(|c| c.name == name).cloned().unwrap();

        let id = by_name("id");
        assert_eq!(id.primary_key, 1);

        let parent_id = by_name("parent_id");
        assert!(parent_id.not_null);
        assert_eq!(parent_id.primary_key, 0);

        let note = by_name("note");
        assert!(!note.not_null);
        assert_eq!(note.default_value.as_deref(), Some("'hi'"));
    }

    #[test]
    fn table_columns_empty_for_unknown_table() {
        let conn = schema_conn();
        assert!(table_columns(&conn, "main", "nope").unwrap().is_empty());
    }

    #[test]
    fn table_foreign_keys_reports_relationship() {
        let conn = schema_conn();
        let fks = table_foreign_keys(&conn, "main", "child").unwrap();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].table, "parent");
        assert_eq!(fks[0].from, "parent_id");
        assert_eq!(fks[0].to.as_deref(), Some("id"));
    }

    #[test]
    fn table_indexes_reports_columns_and_uniqueness() {
        let conn = schema_conn();
        let indexes = table_indexes(&conn, "main", "child").unwrap();

        let note_idx = indexes
            .iter()
            .find(|i| i.name == "idx_child_note")
            .expect("idx_child_note should exist");
        assert!(!note_idx.unique);
        assert_eq!(note_idx.columns, vec!["note".to_string()]);

        let unique_idx = indexes
            .iter()
            .find(|i| i.name == "idx_child_unique")
            .expect("idx_child_unique should exist");
        assert!(unique_idx.unique);
        assert_eq!(
            unique_idx.columns,
            vec!["parent_id".to_string(), "note".to_string()]
        );
    }

    #[test]
    fn table_create_sql_returns_statement() {
        let conn = schema_conn();
        let sql = table_create_sql(&conn, "main", "parent")
            .unwrap()
            .expect("create sql for an existing table");
        assert!(sql.contains("CREATE TABLE"));
        assert!(sql.contains("parent"));

        assert!(table_create_sql(&conn, "main", "nope").unwrap().is_none());
    }

    #[test]
    fn describe_returns_full_description() {
        let conn = schema_conn();
        let desc = describe(&conn, "main", "child")
            .unwrap()
            .expect("child table should be described");
        assert_eq!(desc.schema, "main");
        assert_eq!(desc.table, "child");
        assert_eq!(desc.columns.len(), 3);
        assert_eq!(desc.foreign_keys.len(), 1);
        assert!(desc.create_sql.is_some());
        assert!(desc.indexes.iter().any(|i| i.name == "idx_child_note"));
    }

    #[test]
    fn describe_returns_none_for_unknown_table() {
        let conn = schema_conn();
        assert!(describe(&conn, "main", "missing").unwrap().is_none());
    }

    // --- tool entry points (exercise the global registry) ---------------

    #[test]
    fn connect_rejects_duplicate_alias() {
        let alias = "test_dup_alias";
        drop_conn(alias);
        open_memory(alias);

        let dup = connect(req(
            "connect",
            json!({ "alias": alias, "path": ":memory:", "mode": "write" }),
        ));
        assert_eq!(dup.is_error, Some(true));
        assert!(text(&dup).contains("already exists"));

        drop_conn(alias);
    }

    #[test]
    fn connect_read_only_missing_file_fails() {
        let alias = "test_ro_missing_alias";
        drop_conn(alias);

        let res = connect(req(
            "connect",
            json!({
                "alias": alias,
                "path": "/nonexistent-dir-xyz/missing.sqlite",
                "mode": "read"
            }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("Failed to open"));
        // Nothing should have been registered on failure.
        assert!(!registry().contains_key(alias));
    }

    #[test]
    fn connect_create_makes_database_and_parent_dirs_in_write_mode() {
        let alias = "test_create_alias";
        drop_conn(alias);

        let dir = std::env::temp_dir().join("sqlite_plugin_create_test_a/nested");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("sqlite_plugin_create_test_a"));
        let path = dir.join("created.sqlite");
        let path_str = path.to_str().unwrap().to_string();

        let res = connect(req(
            "connect",
            json!({ "alias": alias, "path": path_str, "mode": "read", "create": true }),
        ));
        assert!(is_ok(&res), "create should succeed: {}", text(&res));

        // Opened in write mode regardless of any `mode` argument, file exists.
        let info: ConnectResponse = structured(&res);
        assert_eq!(info.mode, AccessMode::Write);
        assert!(path.exists());
        assert!(registry().contains_key(alias));

        drop_conn(alias);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("sqlite_plugin_create_test_a"));
    }

    #[test]
    fn connect_create_fails_if_file_exists() {
        let alias = "test_create_exists_alias";
        drop_conn(alias);

        let dir = std::env::temp_dir().join("sqlite_plugin_create_test_b");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("existing.sqlite");
        std::fs::write(&path, b"not really a db").unwrap();
        let path_str = path.to_str().unwrap().to_string();

        let res = connect(req(
            "connect",
            json!({ "alias": alias, "path": path_str, "create": true }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("already exists"));
        // Nothing registered on failure.
        assert!(!registry().contains_key(alias));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_is_idempotent() {
        let alias = "test_close_idem_alias";
        drop_conn(alias);

        // Closing an unknown alias succeeds and returns "Closed".
        let res = close(req("close", json!({ "alias": alias })));
        assert!(is_ok(&res));
        assert_eq!(text(&res), "Closed");
        assert!(res.structured_content.is_none());

        // After opening, closing also returns "Closed" and removes it.
        open_memory(alias);
        let res = close(req("close", json!({ "alias": alias })));
        assert!(is_ok(&res));
        assert_eq!(text(&res), "Closed");
        assert!(!registry().contains_key(alias));
    }

    #[test]
    fn connections_lists_open_alias() {
        let alias = "test_conn_list_alias";
        drop_conn(alias);
        open_memory(alias);

        let res = connections(req("connections", json!({})));
        let resp: ConnectionsResponse = structured(&res);
        let entry = resp
            .connections
            .iter()
            .find(|c| c.alias == alias)
            .expect("our alias should be listed");
        assert_eq!(entry.mode, AccessMode::Write);
        assert_eq!(entry.path, ":memory:");

        drop_conn(alias);
    }

    #[test]
    fn tables_errors_for_unknown_alias() {
        let res = tables(req(
            "tables",
            json!({ "alias": "definitely_not_open_alias" }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("does not exist"));
    }

    #[test]
    fn describe_table_errors_for_unknown_alias() {
        let res = describe_table(req(
            "describe_table",
            json!({ "alias": "definitely_not_open_alias", "table": "t" }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("does not exist"));
    }

    #[test]
    fn execute_sql_errors_for_unknown_alias() {
        let res = execute_sql(req(
            "execute_sql",
            json!({ "alias": "definitely_not_open_alias", "sql": "SELECT 1" }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("does not exist"));
    }

    #[test]
    fn end_to_end_flow_through_call_tool() {
        let alias = "test_e2e_alias";
        drop_conn(alias);

        // connect
        let res = call_tool(req(
            "connect",
            json!({ "alias": alias, "path": ":memory:", "mode": "write" }),
        ))
        .unwrap();
        assert!(is_ok(&res));

        // create + insert
        let res = call_tool(req(
            "execute_sql",
            json!({ "alias": alias, "sql": "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)" }),
        ))
        .unwrap();
        assert!(is_ok(&res));

        let res = call_tool(req(
            "execute_sql",
            json!({ "alias": alias, "sql": "INSERT INTO t (name) VALUES ('a'), ('b')" }),
        ))
        .unwrap();
        let resp = as_execute(structured(&res));
        assert_eq!(resp.rows_affected, 2);

        // query
        let res = call_tool(req(
            "execute_sql",
            json!({ "alias": alias, "sql": "SELECT id, name FROM t ORDER BY id" }),
        ))
        .unwrap();
        let resp = as_query(structured(&res));
        assert_eq!(resp.rows.len(), 2);

        // tables
        let res = call_tool(req("tables", json!({ "alias": alias }))).unwrap();
        let resp: TablesResponse = structured(&res);
        assert!(
            resp.tables
                .iter()
                .any(|t| t.name == "t" && t.r#type == "table")
        );

        // describe_table
        let res = call_tool(req(
            "describe_table",
            json!({ "alias": alias, "table": "t" }),
        ))
        .unwrap();
        let resp: DescribeTableResponse = structured(&res);
        assert_eq!(resp.alias, alias);
        assert_eq!(resp.columns.len(), 2);

        // describe a missing table -> error
        let res = call_tool(req(
            "describe_table",
            json!({ "alias": alias, "table": "missing" }),
        ))
        .unwrap();
        assert_eq!(res.is_error, Some(true));

        // close
        let res = call_tool(req("close", json!({ "alias": alias }))).unwrap();
        assert!(is_ok(&res));
        assert_eq!(text(&res), "Closed");
    }

    #[test]
    fn attach_errors_for_unknown_alias() {
        let res = attach(req(
            "attach",
            json!({
                "alias": "definitely_not_open_alias",
                "db_name": "aux",
                "path": ":memory:"
            }),
        ));
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("does not exist"));
    }

    #[test]
    fn attach_adds_schema_visible_to_tables() {
        let alias = "test_attach_alias";
        drop_conn(alias);
        open_memory(alias);

        let aux_path = std::env::temp_dir().join(format!(
            "sqlite_plugin_attach_test_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&aux_path);
        let aux_path_str = aux_path.to_str().unwrap().to_string();

        let res = attach(req(
            "attach",
            json!({
                "alias": alias,
                "db_name": "aux",
                "path": aux_path_str,
                "mode": "write"
            }),
        ));
        assert!(is_ok(&res), "attach failed: {}", text(&res));
        let resp: AttachResponse = structured(&res);
        assert_eq!(resp.db_name, "aux");
        assert_eq!(resp.mode, AccessMode::Write);

        // Create a table in the attached schema and confirm `tables` sees it.
        let res = execute_sql(req(
            "execute_sql",
            json!({ "alias": alias, "sql": "CREATE TABLE aux.widgets (id INTEGER)" }),
        ));
        assert!(is_ok(&res), "create in aux failed: {}", text(&res));

        let res = tables(req("tables", json!({ "alias": alias })));
        let resp: TablesResponse = structured(&res);
        assert!(
            resp.tables
                .iter()
                .any(|t| t.schema == "aux" && t.name == "widgets"),
            "attached table should be listed under schema 'aux': {:?}",
            resp.tables
        );

        drop_conn(alias);
        let _ = std::fs::remove_file(&aux_path);
    }

    #[test]
    fn call_tool_unknown_tool_errors() {
        let res = call_tool(req("does_not_exist", json!({}))).unwrap();
        assert_eq!(res.is_error, Some(true));
        assert!(text(&res).contains("Unknown tool"));
    }

    // --- tool listing and default handlers ------------------------------

    #[test]
    fn list_tools_exposes_all_tools() {
        let result = list_tools(ListToolsRequest::default()).unwrap();
        let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
        for expected in [
            "connect",
            "attach",
            "close",
            "connections",
            "tables",
            "describe_table",
            "execute_sql",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
        assert_eq!(result.tools.len(), 7);

        // Every tool advertises an output schema except `close`, which returns a
        // plain "Closed" text result.
        for tool in &result.tools {
            if tool.name == "close" {
                assert!(tool.output_schema.is_none());
            } else {
                assert!(
                    tool.output_schema.is_some(),
                    "tool {} should have an output schema",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn default_handlers_behaviour() {
        let complete_req = CompleteRequest {
            context: PluginRequestContext::default(),
            request: CompleteRequestParam::default(),
        };
        assert!(complete(complete_req).is_ok());
        assert!(list_prompts(ListPromptsRequest::default()).is_ok());
        assert!(list_resources(ListResourcesRequest::default()).is_ok());
        assert!(list_resource_templates(ListResourceTemplatesRequest::default()).is_ok());
        assert!(on_roots_list_changed(PluginNotificationContext::default()).is_ok());
        assert!(get_prompt(GetPromptRequest::default()).is_err());
        assert!(read_resource(ReadResourceRequest::default()).is_err());
    }
}
