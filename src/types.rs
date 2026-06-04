//! Input (argument) and output (response) types for the SQLite plugin tools.
//!
//! Every argument struct derives [`schemars::JsonSchema`] so it can be turned
//! into a tool `inputSchema` via `schema_for!`, and every response struct does
//! the same for `outputSchema`. Responses are also serialized into the
//! `structuredContent` of the `CallToolResult` (with a pretty-printed copy in
//! the text content), following the Context7 plugin's typing conventions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a database is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    /// Open read-only. The database file must already exist.
    #[default]
    Read,
    /// Open read-write, creating the database file if it does not exist.
    Write,
}

impl AccessMode {
    /// Human-readable label (`"read"` / `"write"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AccessMode::Read => "read",
            AccessMode::Write => "write",
        }
    }

    /// SQLite URI `mode=` value used by `ATTACH`.
    pub fn uri_mode(self) -> &'static str {
        match self {
            AccessMode::Read => "ro",
            AccessMode::Write => "rwc",
        }
    }
}

// --- connect -------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectArguments {
    #[schemars(description = "Unique alias to register this connection under. \
        Fails if the alias is already in use.")]
    pub alias: String,

    #[schemars(
        description = "Filesystem path to the SQLite database file. Must be reachable \
        from within the plugin sandbox (see the plugin's allowed_paths configuration)."
    )]
    pub path: String,

    #[schemars(
        description = "Open mode: 'read' (read-only, the file must exist) or 'write' \
        (read-write, creating the file if needed). Defaults to 'read'. Ignored when \
        `create` is true."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AccessMode>,

    #[schemars(
        description = "When true, create a brand-new database at `path` and open it in \
        write mode, creating any missing parent directories. Fails if a file already \
        exists at `path` (never overwrites an existing database). Overrides `mode`. \
        Defaults to false."
    )]
    #[serde(default)]
    pub create: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectResponse {
    pub alias: String,
    pub path: String,
    pub mode: AccessMode,
    pub message: String,
}

// --- attach --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachArguments {
    #[schemars(
        description = "Alias of an existing connection to attach the database to. \
        Fails if the alias does not exist."
    )]
    pub alias: String,

    #[schemars(
        description = "Schema name to attach the database as (used in the SQL `AS` clause). \
        Referenced as a schema qualifier, e.g. `SELECT * FROM <db_name>.<table>`."
    )]
    pub db_name: String,

    #[schemars(description = "Filesystem path to the SQLite database file to attach.")]
    pub path: String,

    #[schemars(
        description = "Open mode for the attached database: 'read' (read-only) or 'write' \
        (read-write, creating the file if needed). Defaults to 'read'."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AccessMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachResponse {
    pub alias: String,
    pub db_name: String,
    pub path: String,
    pub mode: AccessMode,
    pub message: String,
}

// --- close ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloseArguments {
    #[schemars(
        description = "Alias of the connection to close. Closing is idempotent: closing an \
        alias that is not open still succeeds."
    )]
    pub alias: String,
}

// `close` returns a plain "Closed" text result, so it has no response struct.

// --- connections ---------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConnectionsArguments {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectionInfo {
    pub alias: String,
    pub path: String,
    pub mode: AccessMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectionsResponse {
    pub connections: Vec<ConnectionInfo>,
}

// --- tables --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TablesArguments {
    #[schemars(
        description = "Alias of the connection to list tables for. Includes tables and \
        views from the main database and any attached databases."
    )]
    pub alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TableEntry {
    /// Schema the object lives in (`main`, `temp`, or an attached schema name).
    pub schema: String,
    pub name: String,
    /// `table` or `view`.
    #[serde(rename = "type")]
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TablesResponse {
    pub alias: String,
    pub tables: Vec<TableEntry>,
}

// --- describe_table ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DescribeTableArguments {
    #[schemars(description = "Alias of the connection to inspect.")]
    pub alias: String,

    #[schemars(description = "Name of the table (or view) to describe.")]
    pub table: String,

    #[schemars(
        description = "Schema/database name the table lives in, for attached databases. \
        Defaults to 'main'."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ColumnInfo {
    /// Column index within the table.
    pub cid: i64,
    pub name: String,
    /// Declared type (may be empty for columns without a declared type).
    #[serde(rename = "type")]
    pub r#type: String,
    pub not_null: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// Position in the primary key (0 if the column is not part of the primary key).
    pub primary_key: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ForeignKeyInfo {
    pub id: i64,
    pub seq: i64,
    /// Referenced (parent) table.
    pub table: String,
    /// Local column.
    pub from: String,
    /// Referenced column (null when the parent's primary key is used implicitly).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
    #[serde(rename = "match")]
    pub r#match: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IndexInfo {
    pub name: String,
    pub unique: bool,
    /// How the index was created: `c` (CREATE INDEX), `u` (UNIQUE), or `pk` (PRIMARY KEY).
    pub origin: String,
    pub partial: bool,
    /// Indexed columns, in index order. Expression columns are reported as `<expr>`.
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DescribeTableResponse {
    pub alias: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub indexes: Vec<IndexInfo>,
    /// The `CREATE TABLE`/`CREATE VIEW` statement from `sqlite_master`, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_sql: Option<String>,
}

// --- execute_sql ---------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteSqlArguments {
    #[schemars(description = "Alias of the connection to run the SQL against.")]
    pub alias: String,

    #[schemars(
        description = "A single SQL statement to execute. The plugin inspects the prepared \
        statement's column count to decide whether to run it as a query (returns columns and rows) \
        or as a non-query statement (returns the number of affected rows)."
    )]
    pub sql: String,
}

/// The outcome of running a single SQL statement.
///
/// This is a tagged union: the `result_type` discriminator selects between a
/// query result set and the affected-row count of a non-query statement, and
/// only the fields relevant to that case are present. Serde's internal tagging
/// keeps the wire format flat, e.g.
/// `{ "result_type": "query", "columns": [...], "rows": [...] }` or
/// `{ "result_type": "execute", "rows_affected": 3 }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "result_type", rename_all = "lowercase")]
pub enum ExecuteSqlResponse {
    /// The statement returned a result set.
    Query(QueryResult),
    /// The statement modified the database.
    Execute(ExecuteResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueryResult {
    /// Column names for the result set.
    pub columns: Vec<String>,
    /// Row values, as an array of rows where each row is an array of values
    /// aligned with `columns`. BLOB values are encoded as `{ "$base64": "<data>" }`.
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteResult {
    /// Number of rows affected by the statement.
    pub rows_affected: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- AccessMode ------------------------------------------------------

    #[test]
    fn access_mode_default_is_read() {
        assert_eq!(AccessMode::default(), AccessMode::Read);
    }

    #[test]
    fn access_mode_as_str() {
        assert_eq!(AccessMode::Read.as_str(), "read");
        assert_eq!(AccessMode::Write.as_str(), "write");
    }

    #[test]
    fn access_mode_uri_mode() {
        assert_eq!(AccessMode::Read.uri_mode(), "ro");
        assert_eq!(AccessMode::Write.uri_mode(), "rwc");
    }

    #[test]
    fn access_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(AccessMode::Read).unwrap(),
            json!("read")
        );
        assert_eq!(
            serde_json::to_value(AccessMode::Write).unwrap(),
            json!("write")
        );
    }

    #[test]
    fn access_mode_deserializes_lowercase() {
        assert_eq!(
            serde_json::from_value::<AccessMode>(json!("read")).unwrap(),
            AccessMode::Read
        );
        assert_eq!(
            serde_json::from_value::<AccessMode>(json!("write")).unwrap(),
            AccessMode::Write
        );
    }

    #[test]
    fn access_mode_rejects_unknown_variant() {
        assert!(serde_json::from_value::<AccessMode>(json!("append")).is_err());
    }

    // --- argument deserialization ---------------------------------------

    #[test]
    fn connect_arguments_mode_defaults_to_none() {
        let args: ConnectArguments =
            serde_json::from_value(json!({ "alias": "a", "path": "/tmp/db" })).unwrap();
        assert_eq!(args.alias, "a");
        assert_eq!(args.path, "/tmp/db");
        assert!(args.mode.is_none());
    }

    #[test]
    fn connect_arguments_parses_mode() {
        let args: ConnectArguments =
            serde_json::from_value(json!({ "alias": "a", "path": "/tmp/db", "mode": "write" }))
                .unwrap();
        assert_eq!(args.mode, Some(AccessMode::Write));
    }

    #[test]
    fn connect_arguments_requires_alias_and_path() {
        assert!(serde_json::from_value::<ConnectArguments>(json!({ "path": "/tmp/db" })).is_err());
        assert!(serde_json::from_value::<ConnectArguments>(json!({ "alias": "a" })).is_err());
    }

    #[test]
    fn attach_arguments_parse() {
        let args: AttachArguments = serde_json::from_value(json!({
            "alias": "a",
            "db_name": "aux",
            "path": "/tmp/aux.db"
        }))
        .unwrap();
        assert_eq!(args.alias, "a");
        assert_eq!(args.db_name, "aux");
        assert_eq!(args.path, "/tmp/aux.db");
        assert!(args.mode.is_none());
    }

    #[test]
    fn describe_table_arguments_db_name_optional() {
        let args: DescribeTableArguments =
            serde_json::from_value(json!({ "alias": "a", "table": "t" })).unwrap();
        assert!(args.db_name.is_none());

        let args: DescribeTableArguments =
            serde_json::from_value(json!({ "alias": "a", "table": "t", "db_name": "aux" }))
                .unwrap();
        assert_eq!(args.db_name.as_deref(), Some("aux"));
    }

    // --- response / field serialization ---------------------------------

    #[test]
    fn connect_response_round_trips() {
        let resp = ConnectResponse {
            alias: "a".into(),
            path: "/tmp/db".into(),
            mode: AccessMode::Write,
            message: "ok".into(),
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["mode"], json!("write"));
        assert_eq!(value["alias"], json!("a"));
    }

    #[test]
    fn table_entry_renames_type_field() {
        let entry = TableEntry {
            schema: "main".into(),
            name: "t".into(),
            r#type: "table".into(),
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["type"], json!("table"));
        assert!(value.get("r#type").is_none());
    }

    #[test]
    fn column_info_omits_none_default_value() {
        let col = ColumnInfo {
            cid: 0,
            name: "id".into(),
            r#type: "INTEGER".into(),
            not_null: true,
            default_value: None,
            primary_key: 1,
        };
        let value = serde_json::to_value(&col).unwrap();
        assert!(value.get("default_value").is_none());
        assert_eq!(value["type"], json!("INTEGER"));
        assert_eq!(value["not_null"], json!(true));
    }

    #[test]
    fn column_info_includes_some_default_value() {
        let col = ColumnInfo {
            cid: 1,
            name: "note".into(),
            r#type: "TEXT".into(),
            not_null: false,
            default_value: Some("'hi'".into()),
            primary_key: 0,
        };
        let value = serde_json::to_value(&col).unwrap();
        assert_eq!(value["default_value"], json!("'hi'"));
    }

    #[test]
    fn foreign_key_info_omits_none_to_and_renames_match() {
        let fk = ForeignKeyInfo {
            id: 0,
            seq: 0,
            table: "parent".into(),
            from: "parent_id".into(),
            to: None,
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
            r#match: "NONE".into(),
        };
        let value = serde_json::to_value(&fk).unwrap();
        assert!(value.get("to").is_none());
        assert_eq!(value["match"], json!("NONE"));
    }

    #[test]
    fn describe_table_response_omits_none_create_sql() {
        let resp = DescribeTableResponse {
            alias: "a".into(),
            schema: "main".into(),
            table: "t".into(),
            columns: vec![],
            foreign_keys: vec![],
            indexes: vec![],
            create_sql: None,
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert!(value.get("create_sql").is_none());
    }

    #[test]
    fn execute_sql_response_query_serialization() {
        let resp = ExecuteSqlResponse::Query(QueryResult {
            columns: vec!["a".into()],
            rows: vec![vec![json!(1)]],
        });
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["result_type"], json!("query"));
        assert_eq!(value["columns"], json!(["a"]));
        assert_eq!(value["rows"], json!([[1]]));
        // The execute-only field is absent for a query result.
        assert!(value.get("rows_affected").is_none());
    }

    #[test]
    fn execute_sql_response_execute_serialization() {
        let resp = ExecuteSqlResponse::Execute(ExecuteResult { rows_affected: 5 });
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["result_type"], json!("execute"));
        assert_eq!(value["rows_affected"], json!(5));
        // The query-only fields are absent for an execute result.
        assert!(value.get("columns").is_none());
        assert!(value.get("rows").is_none());
    }

    #[test]
    fn execute_sql_response_round_trips() {
        for resp in [
            ExecuteSqlResponse::Query(QueryResult {
                columns: vec!["a".into(), "b".into()],
                rows: vec![vec![json!(1), Value::Null]],
            }),
            ExecuteSqlResponse::Execute(ExecuteResult { rows_affected: 3 }),
        ] {
            let value = serde_json::to_value(&resp).unwrap();
            let back: ExecuteSqlResponse = serde_json::from_value(value).unwrap();
            assert_eq!(back, resp);
        }
    }
}
