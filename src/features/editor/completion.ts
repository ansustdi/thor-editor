import type * as Monaco from "monaco-editor/editor/editor.api";
import { useApp, type Connection } from "../../store/app";
import { api, isSql, type ColumnInfo, type DbObject, type SqlAnalysis } from "../../lib/ipc";
import { statementAt } from "../../lib/sql";
import { analyzeOne, derivedFor, PROBE, tableAliases } from "./analysis";

const KEYWORDS = (
  "SELECT FROM WHERE AND OR NOT IN IS NULL LIKE BETWEEN EXISTS JOIN INNER LEFT RIGHT FULL OUTER CROSS ON USING GROUP BY " +
  "ORDER HAVING LIMIT OFFSET FETCH FIRST ROWS ONLY DISTINCT AS CASE WHEN THEN ELSE END UNION ALL INTERSECT EXCEPT MINUS " +
  "INSERT INTO VALUES UPDATE SET DELETE MERGE CREATE ALTER DROP TRUNCATE TABLE VIEW INDEX SEQUENCE PRIMARY KEY FOREIGN " +
  "REFERENCES DEFAULT CONSTRAINT UNIQUE CHECK COMMIT ROLLBACK BEGIN WITH RECURSIVE OVER PARTITION COUNT SUM AVG MIN MAX " +
  "COALESCE NULLIF CAST EXPLAIN ASC DESC GRANT REVOKE PROCEDURE FUNCTION TRIGGER RETURNING"
).split(" ");

const CLAUSE_STOP = new Set([
  "where", "on", "join", "left", "right", "inner", "outer", "full", "cross", "group", "order", "having", "set", "values",
  "limit", "union", "minus", "except", "intersect", "connect", "start", "fetch", "offset", "using", "natural", "select",
  "partition", "sample", "final", "prewhere", "format", "settings", "with",
]);

let registered = false;

/** Active connection key per editor model, set by the editor component. */
export const modelConn = new Map<string, string>();

export interface TableRef {
  /** As written, unquoted: "table" or "schema.table". */
  table: string;
  alias?: string;
}

const IDENT = String.raw`(?:"[^"]+"|\x60[^\x60]+\x60|\[[^\]]+\]|[A-Za-z_][\w$#]*)(?:\s*\.\s*(?:"[^"]+"|\x60[^\x60]+\x60|\[[^\]]+\]|[A-Za-z_][\w$#]*))?`;
const unquote = (s: string) => s.replace(/\s*\.\s*/g, ".").replace(/["`[\]]/g, "");

/** Tables referenced by FROM / JOIN / UPDATE / INTO, including comma lists (`from a x, b y`). */
export function tableRefs(text: string): TableRef[] {
  const out: TableRef[] = [];
  const re = new RegExp(String.raw`(\bfrom|\bjoin|\bupdate|\binto|,)\s+(${IDENT})(?:\s+(?:as\s+)?([A-Za-z_][\w$#]*))?`, "gi");
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    const table = unquote(m[2]);
    if (CLAUSE_STOP.has(table.toLowerCase())) continue;
    const alias = m[3] && !CLAUSE_STOP.has(m[3].toLowerCase()) ? m[3] : undefined;
    out.push({ table, alias });
    // let a following ", next_table" be matched by the comma branch
    if (m[3] && !alias) re.lastIndex -= m[3].length;
  }
  return out;
}

/** alias / table short name (lower-case) → table as written. */
export function aliasMap(text: string): Map<string, string> {
  const m = new Map<string, string>();
  for (const r of tableRefs(text)) {
    m.set(r.table.split(".").pop()!.toLowerCase(), r.table);
    if (r.alias) m.set(r.alias.toLowerCase(), r.table);
  }
  return m;
}

// ---------------------------------------------------------------- metadata (cached)

const schemaCache = new Map<string, Promise<string[]>>();

export function schemasOf(conn: Connection): Promise<string[]> {
  let p = schemaCache.get(conn.info.sessionId);
  if (!p) {
    p = api.listSchemas(conn.info.sessionId).catch(() => []);
    schemaCache.set(conn.info.sessionId, p);
  }
  return p;
}

const inflight = new Map<string, Promise<DbObject[]>>();

export async function objectsOf(conn: Connection, schema: string): Promise<DbObject[]> {
  const st = useApp.getState();
  const cached = st.objects[conn.key]?.[schema];
  if (cached) return cached;
  const k = `${conn.key}\u0001${schema}`;
  let p = inflight.get(k);
  if (!p) {
    p = api
      .listObjects(conn.info.sessionId, schema)
      .then((list) => {
        useApp.getState().cacheObjects(conn.key, schema, list);
        return list;
      })
      .catch(() => [] as DbObject[])
      .finally(() => inflight.delete(k));
    inflight.set(k, p);
  }
  return p;
}

export async function columnsOf(conn: Connection, schema: string, table: string): Promise<ColumnInfo[]> {
  const key = `${schema}.${table}`;
  const cached = useApp.getState().columns[conn.key]?.[key];
  if (cached) return cached;
  try {
    const cols = await api.tableColumns(conn.info.sessionId, schema, table);
    useApp.getState().cacheColumns(conn.key, key, cols);
    return cols;
  } catch {
    return [];
  }
}

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Resolves a table as written to (schema, real name) using the schema list / object caches. */
export async function locate(conn: Connection, written: string): Promise<{ schema: string; name: string } | null> {
  const parts = written.split(".");
  const name = parts.pop()!;
  const schemas = await schemasOf(conn);
  const schema = parts.length ? schemas.find((s) => eq(s, parts[0])) ?? parts[0] : conn.info.currentSchema;
  const list = await objectsOf(conn, schema);
  const hit = list.find((o) => eq(o.name, name));
  if (hit) return { schema, name: hit.name };
  // unqualified name living in another already-loaded schema
  if (!parts.length) {
    for (const [s, l] of Object.entries(useApp.getState().objects[conn.key] ?? {})) {
      const h = l.find((o) => eq(o.name, name));
      if (h) return { schema: s, name: h.name };
    }
  }
  return null;
}

/** Output columns of a CTE / subquery; `*` expands to the columns of its source tables (or CTEs). */
export async function derivedColumns(conn: Connection, a: SqlAnalysis, d: SqlAnalysis["derived"][number], depth = 0): Promise<string[]> {
  const cols = [...d.columns];
  if (d.star && depth < 4) {
    for (const src of d.sources) {
      const inner = !src.includes(".") ? a.derived.find((x) => x.kind === "cte" && eq(x.name, src) && x !== d) : undefined;
      const more = inner
        ? await derivedColumns(conn, a, inner, depth + 1)
        : await (async () => {
            const loc = await locate(conn, src);
            return loc ? (await columnsOf(conn, loc.schema, loc.name)).map((c) => c.name) : [];
          })();
      for (const c of more) if (!cols.some((x) => eq(x, c))) cols.push(c);
    }
  }
  return cols;
}

// ---------------------------------------------------------------- provider

type Ctx = "table" | "column" | "other";

/** What belongs at the cursor: a table name (after FROM/JOIN/…), a column, or anything. */
export function contextOf(before: string): Ctx {
  const t = before.replace(/'(?:[^']|'')*'/g, "''").replace(/--[^\n]*/g, "");
  if (new RegExp(String.raw`\b(from|join|update|into|table|describe|desc|truncate)\s+(${IDENT}(\s+(as\s+)?[A-Za-z_][\w$#]*)?\s*,\s*)*[\w$#."\x60]*$`, "i").test(t)) {
    return "table";
  }
  const kw = /\b(select|where|and|or|on|by|having|set|when|then|else|returning|distinct)\b[^;]*$/i.exec(t);
  if (kw || /[(,=<>+\-*/]\s*[\w$#]*$/.test(t)) return "column";
  return "other";
}

/** Kibana Dev Tools style: `GET index/_search` request lines, JSON bodies and SQL. */
function registerEsConsole(monaco: typeof Monaco) {
  monaco.languages.register({ id: "esconsole" });
  monaco.languages.setLanguageConfiguration("esconsole", {
    comments: { lineComment: "#" },
    brackets: [["{", "}"], ["[", "]"], ["(", ")"]],
    autoClosingPairs: [
      { open: "{", close: "}" },
      { open: "[", close: "]" },
      { open: "(", close: ")" },
      { open: '"', close: '"' },
      { open: "'", close: "'" },
    ],
  });
  monaco.languages.setMonarchTokensProvider("esconsole", {
    ignoreCase: true,
    sqlKeywords: [
      "select", "from", "where", "group", "by", "order", "having", "limit", "and", "or", "not", "as", "asc", "desc", "in",
      "between", "like", "rlike", "is", "null", "match", "query", "show", "tables", "columns", "functions", "describe", "distinct",
      "case", "when", "then", "else", "end", "count", "sum", "avg", "min", "max", "true", "false",
    ],
    tokenizer: {
      root: [
        [/^\s*(GET|POST|PUT|DELETE|HEAD|PATCH)(?=\s)/, { token: "keyword", next: "@path" }],
        [/^\s*(#|\/\/).*$/, "comment"],
        [/--.*$/, "comment"],
        [/"(?:[^"\\]|\\.)*"(?=\s*:)/, "type"],
        [/"""/, { token: "string", next: "@triple" }],
        [/"(?:[^"\\]|\\.)*"/, "string"],
        [/'(?:[^']|'')*'/, "string"],
        [/-?\d+(\.\d+)?([eE][-+]?\d+)?/, "number"],
        [/[A-Za-z_][\w.]*/, { cases: { "@sqlKeywords": "keyword", "@default": "identifier" } }],
        [/[{}[\]()]/, "@brackets"],
      ],
      path: [
        [/\s+/, ""],
        [/[^\s?]+/, "variable"],
        [/\?\S*/, { token: "attribute.name", next: "@pop" }],
        [/$/, "", "@pop"],
      ],
      triple: [
        [/"""/, { token: "string", next: "@pop" }],
        [/./, "string"],
      ],
    },
  } as Monaco.languages.IMonarchLanguage);
}

export function registerCompletion(monaco: typeof Monaco) {
  if (registered) return;
  registered = true;
  const K = monaco.languages.CompletionItemKind;
  const objKind = (k: string) =>
    k === "table" || k === "collection" ? K.Struct : k === "view" || k === "materialized view" ? K.Interface : k === "package" ? K.Module : K.Function;

  registerEsConsole(monaco);
  for (const lang of ["sql", "mysql", "pgsql", "redis", "cypher", "javascript", "java", "esconsole"]) {
    monaco.languages.registerCompletionItemProvider(lang, {
      triggerCharacters: [".", " "],
      provideCompletionItems: async (model, position, context) => {
        const connKey = modelConn.get(model.uri.toString());
        const conn = useApp.getState().connections.find((c) => c.key === connKey);
        const word = model.getWordUntilPosition(position);
        const range = {
          startLineNumber: position.lineNumber,
          endLineNumber: position.lineNumber,
          startColumn: word.startColumn,
          endColumn: word.endColumn,
        };
        if (!conn) return { suggestions: [] };
        const dialect = conn.info.dialect;
        const sql = isSql(dialect);

        // Current statement and the text before the cursor within it.
        const full = model.getValue();
        const offset = model.getOffsetAt({ lineNumber: position.lineNumber, column: word.startColumn });
        const stmt = statementAt(full, offset, dialect);
        const stmtText = stmt ? full.slice(stmt.start, Math.max(stmt.end, offset)) : full;
        const before = stmt ? full.slice(stmt.start, offset) : full.slice(0, offset);
        const ctx = sql ? contextOf(before) : "other";

        // Space only opens the popup right after FROM / JOIN / … (DataGrip behaviour).
        if (context.triggerCharacter === " " && ctx !== "table") return { suggestions: [] };

        // Syntax tree of the statement, with a probe identifier at the cursor so it parses while
        // half-typed. Null when it still does not parse: the regex heuristics below take over.
        const rel = offset - (stmt?.start ?? 0);
        const tree = sql && stmt ? await analyzeOne(dialect, stmtText.slice(0, rel) + PROBE + stmtText.slice(rel)) : null;

        // ---- qualifier: "alias." / "table." / "schema."
        const dot = /((?:"[^"]+"|`[^`]+`|[A-Za-z_][\w$#]*))\.$/.exec(before);
        if (dot && sql) {
          const q = unquote(dot[1]);
          const d = tree && derivedFor(tree, q);
          if (tree && d) {
            const cols = await derivedColumns(conn, tree, d);
            return {
              suggestions: cols.map((c, i) => ({
                label: { label: c, description: d.kind === "cte" ? `CTE ${d.name}` : `subquery ${d.name}` },
                kind: K.Field,
                insertText: c,
                sortText: String(i).padStart(4, "0"),
                range,
              })),
            };
          }
          const written = tree ? tableAliases(tree).get(q.toLowerCase()) : aliasMap(stmtText).get(q.toLowerCase());
          if (written) {
            const loc = await locate(conn, written);
            if (loc) {
              const cols = await columnsOf(conn, loc.schema, loc.name);
              return {
                suggestions: cols.map((c, i) => ({
                  label: { label: c.name, description: c.dataType + (c.primaryKey ? " · PK" : "") },
                  kind: K.Field,
                  insertText: c.name,
                  sortText: String(i).padStart(4, "0"),
                  range,
                })),
              };
            }
          }
          const schemas = await schemasOf(conn);
          const schema = schemas.find((s) => eq(s, q));
          if (schema) {
            const list = await objectsOf(conn, schema);
            return {
              suggestions: list.map((o) => ({
                label: { label: o.name, description: o.kind },
                kind: objKind(o.kind),
                insertText: o.name,
                sortText: (o.kind === "table" ? "a" : "b") + o.name,
                range,
              })),
            };
          }
          return { suggestions: [] };
        }

        const suggestions: Monaco.languages.CompletionItem[] = [];
        const current = conn.info.currentSchema;

        // ---- CTE names (only meaningful where a table goes)
        if (ctx === "table") {
          for (const d of tree?.derived ?? []) {
            if (d.kind !== "cte") continue;
            suggestions.push({ label: { label: d.name, description: "CTE" }, kind: K.Struct, insertText: d.name, sortText: "00" + d.name, range });
          }
        }

        // ---- columns of every table in the statement
        if (sql && ctx !== "table") {
          const refs = tree
            ? tree.tables.filter((t) => !t.cte && !t.name.includes(PROBE)).map((t) => ({ table: t.name, alias: t.alias ?? undefined }))
            : tableRefs(stmtText);
          const seen = new Set<string>();
          // CTE / subquery columns
          for (const d of tree?.derived ?? []) {
            if (d.kind === "cte" && !tree!.tables.some((t) => t.cte && eq(t.name, d.name))) continue;
            for (const c of await derivedColumns(conn, tree!, d)) {
              const id = c.toLowerCase();
              if (seen.has(id)) continue;
              seen.add(id);
              suggestions.push({
                label: { label: c, description: `${d.kind === "cte" ? "CTE" : "subquery"} ${d.name}` },
                kind: K.Field,
                insertText: c,
                sortText: "0" + c,
                range,
              });
            }
          }
          for (const r of refs) {
            const loc = await locate(conn, r.table);
            if (!loc) continue;
            const cols = await columnsOf(conn, loc.schema, loc.name);
            for (const c of cols) {
              const id = c.name.toLowerCase();
              if (seen.has(id)) continue;
              seen.add(id);
              suggestions.push({
                label: { label: c.name, description: `${c.dataType} · ${r.alias ?? loc.name}` },
                kind: K.Field,
                insertText: c.name,
                sortText: "0" + c.name,
                range,
              });
            }
          }
        }

        // ---- tables / views / packages: current schema first, then other loaded schemas
        const local = await objectsOf(conn, current);
        for (const o of local) {
          suggestions.push({
            label: { label: o.name, description: o.kind },
            kind: objKind(o.kind),
            insertText: o.name,
            sortText: (ctx === "table" ? "0" : "1") + (o.kind === "table" ? "a" : "b") + o.name,
            range,
          });
        }
        for (const [schema, list] of Object.entries(useApp.getState().objects[conn.key] ?? {})) {
          if (eq(schema, current)) continue;
          for (const o of list) {
            suggestions.push({
              label: { label: `${schema}.${o.name}`, description: o.kind },
              kind: objKind(o.kind),
              insertText: `${schema}.${o.name}`,
              sortText: "2" + o.name,
              range,
            });
          }
        }

        // ---- schema names (type "wh." next to get its tables)
        for (const s of await schemasOf(conn)) {
          suggestions.push({
            label: { label: s, description: "schema" },
            kind: K.Module,
            insertText: s,
            sortText: (ctx === "table" ? "1" : "3") + s,
            range,
          });
        }

        if (ctx !== "table") {
          if (sql) {
            for (const k of KEYWORDS) suggestions.push({ label: k, kind: K.Keyword, insertText: k, range, sortText: "4" + k });
          }
          for (const sn of useApp.getState().snippets) {
            suggestions.push({ label: { label: sn.name, description: "snippet" }, kind: K.Snippet, insertText: sn.sql, range, sortText: "5" + sn.name });
          }
        }
        return { suggestions };
      },
    });
  }
}

/** Forget cached schema names for a session (e.g. after CREATE SCHEMA / DATABASE). */
export function forgetSchemas(sessionId: string) {
  schemaCache.delete(sessionId);
}

/** Loads object lists of schemas referenced as `schema.` in the text (for highlighting / completion). */
export async function preloadReferencedSchemas(conn: Connection, qualifiers: string[]) {
  if (!qualifiers.length) return;
  const schemas = await schemasOf(conn);
  const loaded = useApp.getState().objects[conn.key] ?? {};
  for (const q of new Set(qualifiers.map((x) => x.toLowerCase()))) {
    const real = schemas.find((s) => s.toLowerCase() === q);
    if (real && !loaded[real]) await objectsOf(conn, real);
  }
}
