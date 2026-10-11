import type { ConnectionProfile, Dialect, Engine } from "./ipc";

export interface EngineInfo {
  id: Engine;
  label: string;
  dialect: Dialect;
  defaultPort: number;
  /** Badge color for the engine icon. */
  color: string;
  short: string;
}

export const ENGINES: EngineInfo[] = [
  { id: "oracle", label: "Oracle", dialect: "oracle", defaultPort: 1521, color: "#c74634", short: "ORA" },
  { id: "postgres", label: "PostgreSQL", dialect: "postgres", defaultPort: 5432, color: "#336791", short: "PG" },
  { id: "mysql", label: "MySQL", dialect: "mysql", defaultPort: 3306, color: "#00758f", short: "MY" },
  { id: "mariadb", label: "MariaDB", dialect: "mysql", defaultPort: 3306, color: "#a0522d", short: "MDB" },
  { id: "mssql", label: "SQL Server", dialect: "mssql", defaultPort: 1433, color: "#a91d22", short: "MS" },
  { id: "sqlite", label: "SQLite", dialect: "sqlite", defaultPort: 0, color: "#0f80cc", short: "LITE" },
  { id: "clickhouse", label: "ClickHouse", dialect: "clickhouse", defaultPort: 8123, color: "#d4a106", short: "CH" },
  { id: "mongodb", label: "MongoDB", dialect: "mongodb", defaultPort: 27017, color: "#13aa52", short: "MGO" },
  { id: "redis", label: "Redis", dialect: "redis", defaultPort: 6379, color: "#d82c20", short: "RDS" },
  { id: "neo4j", label: "Neo4j", dialect: "neo4j", defaultPort: 7687, color: "#018bff", short: "NEO" },
  { id: "elasticsearch", label: "Elasticsearch", dialect: "elasticsearch", defaultPort: 9200, color: "#00bfb3", short: "ES" },
  { id: "janusgraph", label: "JanusGraph", dialect: "janusgraph", defaultPort: 8182, color: "#00a99d", short: "JG" },
];

export const engineInfo = (e: Engine): EngineInfo => ENGINES.find((x) => x.id === e) ?? ENGINES[0];

export const TAG_COLORS: { name: string; value: string }[] = [
  { name: "None", value: "" },
  { name: "Red", value: "#dc2626" },
  { name: "Orange", value: "#ea580c" },
  { name: "Yellow", value: "#ca8a04" },
  { name: "Green", value: "#16a34a" },
  { name: "Blue", value: "#2563eb" },
  { name: "Purple", value: "#9333ea" },
  { name: "Gray", value: "#6b7280" },
];

export function newProfile(engine: Engine = "oracle"): ConnectionProfile {
  return {
    id: "",
    name: "",
    engine,
    host: engine === "sqlite" ? "" : "localhost",
    port: engineInfo(engine).defaultPort,
    database: "",
    username: "",
    savePassword: true,
    color: "",
    favorite: false,
    folder: "",
    sslMode: "",
    auth: "sql",
    autocommit: null,
    oracle: {
      method: "direct",
      serviceType: "service",
      service: "",
      tnsAlias: "",
      ldapAlias: "",
      connectAs: "normal",
      clientLibDir: "",
      configDir: "",
    },
    ssh: { enabled: false, host: "", port: 22, user: "", keyPath: "" },
    params: {},
  };
}

/** Toad-style "Database" column text. */
export function describeTarget(p: ConnectionProfile): string {
  if (p.engine === "sqlite") return p.database;
  if (p.engine === "mongodb" && p.params?.uri) return p.params.uri.replace(/\/\/[^@/]*@/, "//");
  if (p.engine === "oracle") {
    if (p.oracle.method === "tns") return p.oracle.tnsAlias;
    if (p.oracle.method === "ldap") return `LDAP:${p.oracle.ldapAlias}`;
    return `${p.host}:${p.port || 1521}/${p.oracle.service || p.database}`;
  }
  const via = p.ssh?.enabled ? ` (via ${p.ssh.host})` : "";
  return `${p.host}:${p.port || engineInfo(p.engine).defaultPort}${p.database ? "/" + p.database : ""}${via}`;
}

export function displayName(p: ConnectionProfile): string {
  return p.name || `${p.username || "?"}@${describeTarget(p)}`;
}
