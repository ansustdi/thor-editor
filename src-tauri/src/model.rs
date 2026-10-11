use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    Mariadb,
    Sqlite,
    Mssql,
    Oracle,
    Clickhouse,
    Redis,
    Mongodb,
    Neo4j,
    Janusgraph,
    Elasticsearch,
}

impl Engine {
    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mysql | Engine::Mariadb => 3306,
            Engine::Mssql => 1433,
            Engine::Oracle => 1521,
            Engine::Sqlite => 0,
            Engine::Clickhouse => 8123,
            Engine::Redis => 6379,
            Engine::Mongodb => 27017,
            Engine::Neo4j => 7687,
            Engine::Janusgraph => 8182,
            Engine::Elasticsearch => 9200,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OracleOptions {
    /// "direct" | "tns" | "ldap"
    pub method: String,
    /// "service" | "sid"
    pub service_type: String,
    pub service: String,
    pub tns_alias: String,
    pub ldap_alias: String,
    /// "normal" | "sysdba" | "sysoper" | "sysasm" | "sysbackup" | "sysdg" | "syskm"
    pub connect_as: String,
    /// Directory containing oci.dll; empty = bundled / PATH lookup.
    pub client_lib_dir: String,
    /// Directory with tnsnames.ora / sqlnet.ora / ldap.ora; empty = derive from client home.
    pub config_dir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshOptions {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub engine: Engine,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub username: String,
    #[serde(default = "yes")]
    pub save_password: bool,
    /// Hex color tag, empty = none.
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub folder: String,
    /// "disable" | "prefer" | "require"
    #[serde(default)]
    pub ssl_mode: String,
    /// MSSQL: "sql" | "windows"
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub autocommit: Option<bool>,
    #[serde(default)]
    pub oracle: OracleOptions,
    #[serde(default)]
    pub ssh: SshOptions,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub last_used: Option<String>,
}

fn yes() -> bool {
    true
}

impl ConnectionProfile {
    pub fn port_or_default(&self) -> u16 {
        if self.port == 0 {
            self.engine.default_port()
        } else {
            self.port
        }
    }

    pub fn autocommit_default(&self) -> bool {
        // Toad-style: Oracle defaults to manual commit, everything else auto.
        self.autocommit.unwrap_or(self.engine != Engine::Oracle)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    pub numeric: bool,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, numeric: bool) -> Self {
        Self { name: name.into(), type_name: type_name.into(), numeric }
    }
}

/// A single typed value. Serialized to the UI as a plain JSON scalar.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Exact decimal kept as text to avoid precision loss.
    Dec(String),
    Text(String),
    /// Hex-encoded binary.
    Bytes(Vec<u8>),
    DateTime(String),
}

const JS_SAFE_INT: i64 = 9_007_199_254_740_991;

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }

    /// Display text used by text-based exporters.
    pub fn to_text(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Bool(b) => b.to_string(),
            Cell::Int(i) => i.to_string(),
            Cell::Float(f) => f.to_string(),
            Cell::Dec(s) | Cell::Text(s) | Cell::DateTime(s) => s.clone(),
            Cell::Bytes(b) => format!("0x{}", hex::encode_upper(b)),
        }
    }
}

impl Serialize for Cell {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Cell::Null => s.serialize_none(),
            Cell::Bool(b) => s.serialize_bool(*b),
            Cell::Int(i) if i.abs() <= JS_SAFE_INT => s.serialize_i64(*i),
            Cell::Int(i) => s.serialize_str(&i.to_string()),
            Cell::Float(f) if f.is_finite() => s.serialize_f64(*f),
            Cell::Float(f) => s.serialize_str(&f.to_string()),
            other => s.serialize_str(&other.to_text()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub result_id: String,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
    pub affected: Option<u64>,
    pub elapsed_ms: u128,
    /// Informational message (e.g. "reconnected automatically").
    pub notice: Option<String>,
    /// Server output produced by the statement (Oracle DBMS_OUTPUT).
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    /// table | view | procedure | function | package | sequence | trigger | index | synonym | type
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub primary_key: bool,
    /// Column comment / description, when the engine has one.
    #[serde(default)]
    pub comment: Option<String>,
}
