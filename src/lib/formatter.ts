import { format } from "sql-formatter";
import type { Dialect } from "./ipc";

type FormatLang = "postgresql" | "mysql" | "sqlite" | "transactsql" | "plsql" | "clickhouse";

const LANG: Partial<Record<Dialect, FormatLang>> = {
  postgres: "postgresql",
  mysql: "mysql",
  sqlite: "sqlite",
  mssql: "transactsql",
  oracle: "plsql",
  clickhouse: "clickhouse",
};

export interface FormatOptions {
  /** "toad": keywords right-aligned in a river, short statements kept on one line. */
  style: "toad" | "standard";
  /** Prepend `/* Formatted on <timestamp> (Thor Editor) *\/` like Toad. */
  header: boolean;
  /** Statements that fit in this many characters stay on one line (toad style). */
  lineWidth?: number;
}

const CASE_SENSITIVE_NAMES = new Set<Dialect>(["clickhouse"]);

/**
 * Words the formatter may upper-case in a case-sensitive dialect: reserved SQL keywords that can't
 * be bare column names. Keywords that double as common column names (`rows`, `database`, `table`,
 * `engine`, `key`, `partition`, …) are deliberately left out and keep the user's spelling.
 */
const SAFE_KEYWORDS = new Set(
  (
    "SELECT FROM WHERE AND OR NOT AS ON JOIN LEFT RIGHT INNER OUTER FULL CROSS GROUP BY ORDER HAVING LIMIT OFFSET WITH " +
    "UNION ALL DISTINCT CASE WHEN THEN ELSE END IN IS NULL LIKE ILIKE BETWEEN EXISTS INSERT INTO VALUES UPDATE SET DELETE " +
    "CREATE DROP ALTER CAST ASC DESC PREWHERE USING INTERVAL TRUE FALSE EXCEPT INTERSECT OVER"
  ).split(" "),
);

const LITERAL_OR_WORD = /'(?:[^'\\]|\\.|'')*'|"[^"]*"|`[^`]*`|[A-Za-z_][\w$]*/g;

/**
 * Undo case changes the formatter made to anything that isn't a plain SQL keyword (aliases such as
 * `s`, column names that look like keywords). String literals and quoted names are left alone.
 */
function restoreCase(original: string, formatted: string): string {
  const words = (s: string) => (s.match(LITERAL_OR_WORD) ?? []).filter((t) => /^[A-Za-z_]/.test(t));
  const before = words(original);
  // The formatter doesn't add, drop or reorder words, so the n-th word of the output is the n-th word
  // of the input; that lets `rows` the column and `ROWS` the frame clause each keep their own spelling.
  const aligned =
    before.length === words(formatted).length &&
    words(formatted).every((w, i) => w.toLowerCase() === before[i].toLowerCase());
  const spelled = new Map<string, string>();
  for (const w of before) if (!spelled.has(w.toLowerCase())) spelled.set(w.toLowerCase(), w);
  let i = 0;
  return formatted.replace(LITERAL_OR_WORD, (tok) => {
    if (!/^[A-Za-z_]/.test(tok)) return tok; // literal / quoted identifier
    const orig = aligned ? before[i++] : spelled.get(tok.toLowerCase());
    if (!orig || orig === tok) return tok;
    return SAFE_KEYWORDS.has(tok.toUpperCase()) ? tok : orig;
  });
}

const HEADER_RE = /^\s*\/\* Formatted on [^*]*\*\/[ \t]*\r?\n?/;

export function canFormat(d: Dialect): boolean {
  return !!LANG[d];
}

/** Toad's timestamp format: 9/30/2026 9:27:25 AM */
export function formatTimestamp(d = new Date()): string {
  return d.toLocaleString("en-US").replace(",", "");
}

/** Joins a formatted statement onto one line when it is short and has no comments or subqueries. */
function compact(stmt: string, width: number): string {
  const multilineString = stmt.split("\n").some((l) => (l.match(/'/g)?.length ?? 0) % 2 === 1);
  if (/--|\/\*/.test(stmt) || multilineString || /\(\s*select\b/i.test(stmt)) return stmt;
  if (/\b(begin|declare|case)\b/i.test(stmt)) return stmt;
  const one = stmt
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .join(" ")
    .replace(/\s+([,;)])/g, "$1")
    .replace(/\(\s+/g, "(");
  return one.length <= width ? one : stmt;
}

export function formatSql(src: string, dialect: Dialect, opts: FormatOptions): string {
  const language = LANG[dialect];
  if (!language) throw new Error("Formatting is only available for SQL databases");
  const body = src.replace(HEADER_RE, "");
  // ClickHouse function and type names are case-sensitive (toDate, sumIf, LowCardinality(String)):
  // upper-casing them breaks the query, so only keywords are upper-cased there.
  const caseSensitive = CASE_SENSITIVE_NAMES.has(dialect);
  let out = format(body, {
    language,
    keywordCase: "upper",
    functionCase: caseSensitive ? "preserve" : "upper",
    dataTypeCase: caseSensitive ? "preserve" : "upper",
    indentStyle: opts.style === "toad" ? "tabularRight" : "standard",
    tabWidth: 2,
    linesBetweenQueries: 1,
    expressionWidth: 60,
  });
  if (caseSensitive) {
    // sql-formatter doesn't know ClickHouse's camelCase functions and writes `sumIf (x)`.
    out = out.replace(/\b([a-z][A-Za-z0-9_]*[A-Z][A-Za-z0-9_]*) \(/g, "$1(");
    out = restoreCase(body, out);
  }
  if (opts.style === "toad") {
    out = out
      .split(/\n[ \t]*\n/)
      .map((s) => compact(s, opts.lineWidth ?? 80))
      .join("\n\n");
  }
  return opts.header ? `/* Formatted on ${formatTimestamp()} (Thor Editor) */\n${out}` : out;
}
