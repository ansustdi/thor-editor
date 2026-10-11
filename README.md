# Thor Editor

Lightweight multi-database desktop editor built with **Tauri 2 + React + Rust**. Toad-style login manager,
Monaco SQL editor with syntax-aware autocomplete and inspections, canvas result grid, graph view, DataGrip-style export.

All database drivers are native Rust crates compiled into the app, so nothing is downloaded at connect time
(unlike JDBC-based tools). Only Oracle needs a client library, which the installer bundles.

## Supported databases

| Engine | Driver | Notes |
|---|---|---|
| Oracle | rust-oracle (OCI) | Direct (service/SID), TNS (tnsnames.ora), LDAP; SYSDBA etc.; auto-detects installed Oracle homes (registry + PATH, 64-bit only) or a bundled Instant Client in `src-tauri/resources/instantclient` |
| PostgreSQL | tokio-postgres | text protocol → every type rendered like psql |
| MySQL / MariaDB | mysql_async | also works for MySQL-protocol engines (Doris, StarRocks, TiDB) |
| SQL Server | tiberius | SQL or Windows auth |
| SQLite | rusqlite (bundled) | |
| ClickHouse | HTTP interface (reqwest) | streamed JSONCompact, KILL QUERY cancel |
| Redis | redis (async) | redis-cli syntax, key browser by type, per-command result grids |
| MongoDB | mongodb 3 | mongosh syntax: `db.c.find({...}).sort().limit()`, aggregate, insert/update/delete, `show dbs`, `use` |
| Neo4j | neo4rs (Bolt) | Cypher, labels/relationship browser, **graph view** |
| JanusGraph | Gremlin Server WebSocket, GraphSON 3 (tokio-tungstenite) | Gremlin-Groovy scripts, vertex/edge label browser, property keys + `printSchema()`, **graph view**; works with any TinkerPop Gremlin Server. *Database* = traversal source (default `g`), `:use <source>` switches; SASL PLAIN auth |
| Elasticsearch / OpenSearch | REST API (reqwest) | Kibana console requests (`GET idx/_search` + JSON body, NDJSON `_bulk`) and SQL (`_sql` / `_plugins/_sql`, cursor paging) in one editor; hits, aggregation buckets and `_cat` shown as grids; indices / aliases / data streams browser, mapping fields, `PUT index` DDL. Basic auth, or API key in the password field with an empty user; host may be a full cloud URL |

All network engines can connect through an **SSH tunnel** (password or private key; TOFU known_hosts).

Planned: Cassandra, Druid, Pinot, CouchDB, HBase, Hive/Spark/Phoenix/Ignite via a JDBC sidecar.

## Features

- **Saved connections:** search, group by engine/folder/color, favorites, color tags (shown as an underline on the
  connection tab), passwords in Windows Credential Manager, JSON import/export (without passwords).
- **SQL editor:** Ctrl+Enter/F9 statement at cursor, F5 script, Ctrl+E explain plan, Ctrl+Shift+F format,
  `:bind` variable prompt, history, cancel, auto-commit toggle + Commit/Rollback (Oracle defaults to manual commit
  like Toad), confirm UPDATE/DELETE without WHERE, Ctrl+click / F4 opens the object under the cursor.
- **Syntax-aware autocomplete** (parser: [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs)):
  tables, `alias.` columns, CTE and subquery output columns (`*` expanded from the source tables), CTE names,
  snippets; falls back to heuristics while a statement does not parse yet.
- **Inspections while typing** (warnings): syntax errors in queries/DML, unknown tables or views,
  unknown `alias.column` — checked against the loaded schema metadata only.
- **Grid:** sort, filter, copy as TSV/CSV/JSON/INSERT/Markdown/IN-list, value viewer (pretty JSON),
  inline edit of table data → reviewed UPDATE statements (needs primary key).
- **Graph view** (Neo4j, JanusGraph): nodes colored by label, curved directed edges, relationships between the
  returned nodes loaded automatically (so `g.V()` / `MATCH (n) RETURN n` show their edges), click to spotlight
  a node's neighbourhood, property panel with in/out degree, zoom-to-fit.
- **Export:** CSV, TSV, Excel, JSON, JSON Lines, SQL INSERT (batched, per-dialect literals), Markdown, HTML, XML —
  from fetched rows or by re-running the query streamed straight to disk; clipboard.
- **Object browser:** tables/views/procedures/functions/packages/sequences/triggers (indices/aliases/data streams
  for Elasticsearch, keys by type for Redis, labels for graphs); Data / Columns / DDL tabs.
- **ER diagram** per schema (foreign keys incl. composite), drag/pan/zoom, focus filter, SVG export.
- **Import** CSV / TSV / Excel into an existing or new table (column mapping, inferred types, batched INSERTs).
- **Import saved connections** from Toad for Oracle, DBeaver and DataGrip (auto-detected; passwords not imported).
- **Data compare:** two queries (same or different databases) diffed by key columns — changed / only-A / only-B.

## Requirements

- Node.js 20+
- Rust (stable, via [rustup](https://rustup.rs)) and, on Windows, the MSVC C++ Build Tools
- WebView2 (preinstalled on Windows 10/11)
- Docker (only for the integration-test databases)

## Develop

```
npm install
npm run tauri dev        # dev app (Vite on :1430); first run compiles the Rust crates (a few minutes)
npm run fetch:oracle     # once: downloads Oracle Instant Client Basic Light into src-tauri/resources/instantclient
npm run tauri build      # installer in src-tauri/target/release/bundle (runs fetch:oracle automatically)
```
`npm run dev` alone starts only the web UI; database calls need the Tauri backend (`npm run tauri dev`).

### Project layout

```
src/                      React front end
  features/editor/        SQL editor, completion, inspections, explain plan
  features/grid/          result grid, graph view, export
  features/explorer/      object browser, object views, ER diagram
  features/login/         connection manager
  lib/                    IPC bindings, statement splitting, formatter
src-tauri/src/            Rust back end
  drivers/                one module per engine (Session trait in drivers/mod.rs)
  sqlparse.rs             syntax-tree analysis for completion / inspections
  commands.rs             Tauri commands called from the UI
  meta.rs                 catalog queries (schemas, objects, columns, DDL)
```

### Bundled Oracle Instant Client
The installer ships Oracle Instant Client **Basic Light** (unmodified, ~63 MB compressed; Oracle Free Use
Terms and Conditions — see `BASIC_LITE_LICENSE` in that folder), so Oracle connections work on PCs without any
Oracle software. In the login form, *Oracle client → "Bundled Instant Client"* (the default) uses it; an
installed Oracle home can still be chosen instead. TNS aliases are read from `TNS_ADMIN` or from the
`network\admin\tnsnames.ora` of any installed Oracle home. The status bar shows which client is in use.
The client is git-ignored and downloaded by `npm run fetch:oracle`.

## Tests

```
cd src-tauri && cargo test                       # unit + SQLite end-to-end
npm run typecheck                                # front end
docker compose --profile oracle up -d            # PG, MySQL, MariaDB, SQL Server, Oracle, ClickHouse, Redis, Mongo, Neo4j, JanusGraph, Elasticsearch, sshd
DBFORGE_IT=1 DBFORGE_IT_ORACLE=1 cargo test it_ -- --test-threads=1
```
Test DB credentials (local Docker containers only): password `Passw0rd!`; ports PG 55432 (postgres/test),
MySQL 53306 (root/test), MariaDB 53307 (root/test), SQL Server 51433 (sa/master), Oracle 51521 (test / service FREEPDB1),
ClickHouse 58123 (test/test), Redis 56379, MongoDB 57017 (root), Neo4j 57687 (neo4j), JanusGraph 58182 (no auth),
Elasticsearch 59200 (elastic), SSH 52222 (tunnel).
