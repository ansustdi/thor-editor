import type * as Monaco from "monaco-editor/editor/editor.api";
import { isSql, type SqlAnalysis } from "../../lib/ipc";
import { findBindVars, splitStatements } from "../../lib/sql";
import { useApp, type Connection } from "../../store/app";
import { analyze, derivedFor, offsetIn, tableAliases } from "./analysis";
import { columnsOf, derivedColumns, locate, objectsOf, schemasOf } from "./completion";

const OWNER = "sql-inspections";
const DELAY = 700;
const MAX_TEXT = 300_000;
const MAX_STATEMENTS = 400;

/** Statements where the parser's coverage is good enough to report syntax errors. */
const QUERY_LIKE = /^\s*(\(\s*)*(select|with|insert|update|delete|merge|values)\b/i;

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

interface Finding {
  start: number;
  end: number;
  message: string;
}

/** Unknown tables, checked only against object lists of schemas that could be loaded. */
async function tableFindings(conn: Connection, a: SqlAnalysis, at: (line: number, col: number) => number, out: Finding[]) {
  const dialect = conn.info.dialect;
  const schemas = await schemasOf(conn);
  for (const t of a.tables) {
    if (t.cte || eq(t.name, "dual")) continue;
    const parts = t.name.split(".");
    if (parts.length > 2) continue; // catalog.schema.table / linked servers
    const name = parts[parts.length - 1];
    let schema: string | undefined;
    if (parts.length === 2) {
      schema = schemas.find((s) => eq(s, parts[0]));
      if (!schema) continue;
    } else {
      // Oracle public synonyms and PostgreSQL catalog tables resolve outside the schema list.
      if (dialect === "oracle" || (dialect === "postgres" && /^(pg_|information_schema)/i.test(name))) continue;
      schema = conn.info.currentSchema;
    }
    const list = await objectsOf(conn, schema);
    if (!list.length) continue;
    if (list.some((o) => eq(o.name, name))) continue;
    if (parts.length === 1) {
      const loaded = useApp.getState().objects[conn.key] ?? {};
      if (Object.values(loaded).some((l) => l.some((o) => eq(o.name, name)))) continue;
    }
    out.push({
      start: at(t.pos.line, t.pos.column),
      end: at(t.pos.endLine, t.pos.endColumn),
      message: `Unknown table or view \`${t.name}\`${parts.length === 1 ? ` in schema ${schema}` : ""}`,
    });
  }
}

/** `alias.column` where the alias resolves to a known table / CTE / subquery without that column. */
async function columnFindings(conn: Connection, a: SqlAnalysis, at: (line: number, col: number) => number, out: Finding[]) {
  const aliases = tableAliases(a);
  for (const c of a.columns) {
    let known: string[] | null = null;
    let owner = "";
    const d = derivedFor(a, c.qualifier);
    if (d) {
      const cols = await derivedColumns(conn, a, d);
      // A `*` over sources that could not be resolved leaves the column list incomplete.
      if (!cols.length || (d.star && cols.length === d.columns.length)) continue;
      known = cols;
      owner = d.name;
    } else {
      const written = aliases.get(c.qualifier.toLowerCase());
      if (!written) continue;
      const loc = await locate(conn, written);
      if (!loc) continue;
      const cols = await columnsOf(conn, loc.schema, loc.name);
      if (!cols.length) continue;
      known = cols.map((x) => x.name);
      owner = loc.name;
    }
    if (known.some((k) => eq(k, c.name))) continue;
    out.push({
      start: at(c.pos.line, c.pos.column),
      end: at(c.pos.endLine, c.pos.endColumn),
      message: `Unknown column \`${c.name}\` in \`${owner}\``,
    });
  }
}

/**
 * DataGrip-style inspections: syntax errors, unknown tables and unknown `alias.column`
 * references are underlined as warnings while typing.
 */
export function attachInspections(
  ed: Monaco.editor.IStandaloneCodeEditor,
  monaco: typeof Monaco,
  getConn: () => Connection | undefined,
): Monaco.IDisposable {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;

  const run = async () => {
    const model = ed.getModel();
    const conn = getConn();
    if (!model || disposed) return;
    const clear = () => monaco.editor.setModelMarkers(model, OWNER, []);
    if (!conn || !isSql(conn.info.dialect)) return clear();
    const text = model.getValue();
    if (text.length > MAX_TEXT) return clear();
    const version = model.getVersionId();
    const dialect = conn.info.dialect;
    const stmts = splitStatements(text, dialect).slice(0, MAX_STATEMENTS);
    let results: SqlAnalysis[];
    try {
      results = await analyze(dialect, stmts.map((s) => s.sql));
    } catch {
      return;
    }
    const findings: Finding[] = [];
    for (let i = 0; i < stmts.length; i++) {
      const s = stmts[i];
      const a = results[i];
      const at = (line: number, col: number) => offsetIn(s.sql, s.start, line, col);
      if (a.error) {
        // Bind variables / substitution variables are not SQL the parser knows.
        if (!QUERY_LIKE.test(s.sql) || findBindVars(s.sql, dialect).length || (dialect === "oracle" && /&\w/.test(s.sql))) continue;
        const start = Math.min(at(a.error.line, a.error.column), s.end);
        const p = model.getPositionAt(start);
        const word = model.getWordAtPosition(p);
        const end = word ? model.getOffsetAt({ lineNumber: p.lineNumber, column: word.endColumn }) : start + 1;
        findings.push({ start, end, message: `Syntax: ${a.error.message}` });
        continue;
      }
      // DDL names (CREATE TABLE new_t, ALTER …) are relations too, but need not exist yet.
      if (!QUERY_LIKE.test(s.sql)) continue;
      try {
        await tableFindings(conn, a, at, findings);
        await columnFindings(conn, a, at, findings);
      } catch {
        // metadata unavailable: skip this statement
      }
    }
    if (disposed || model.isDisposed() || model.getVersionId() !== version) return;
    monaco.editor.setModelMarkers(
      model,
      OWNER,
      findings.map((f) => {
        const s = model.getPositionAt(f.start);
        const e = model.getPositionAt(Math.max(f.end, f.start + 1));
        return {
          severity: monaco.MarkerSeverity.Warning,
          message: f.message,
          startLineNumber: s.lineNumber,
          startColumn: s.column,
          endLineNumber: e.lineNumber,
          endColumn: e.column,
        };
      }),
    );
  };

  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(run, DELAY);
  };
  const sub = ed.onDidChangeModelContent(schedule);
  schedule();
  return {
    dispose: () => {
      disposed = true;
      clearTimeout(timer);
      sub.dispose();
      const model = ed.getModel();
      if (model) monaco.editor.setModelMarkers(model, OWNER, []);
    },
  };
}
