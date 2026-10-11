import type { CellValue, Dialect } from "./ipc";

export interface Stmt {
  sql: string;
  start: number;
  end: number;
}

function leadingKeywords(s: string, n = 6): string[] {
  const noComments = s.replace(/--[^\n]*|\/\*[\s\S]*?\*\//g, " ");
  return noComments.trim().split(/\s+/).slice(0, n).map((w) => w.toLowerCase());
}

/** Oracle PL/SQL units end with a line holding only "/". */
function isPlsqlStart(s: string): boolean {
  const k = leadingKeywords(s);
  if (k[0] === "begin" || k[0] === "declare") return true;
  if (k[0] !== "create") return false;
  return k.some((w) => ["procedure", "function", "package", "trigger", "type"].includes(w));
}

/**
 * Split a script into statements. Understands quotes, comments, PostgreSQL dollar quoting,
 * Oracle "/" terminated PL/SQL blocks and SQL Server GO batches.
 */
export function splitStatements(text: string, dialect: Dialect): Stmt[] {
  if (dialect === "redis") return splitLines(text);
  if (dialect === "elasticsearch") return splitElastic(text);
  const out: Stmt[] = [];
  let i = 0;
  let start = 0;
  const n = text.length;

  const push = (endExclusive: number, nextStart: number) => {
    const raw = text.slice(start, endExclusive);
    const trimmed = raw.trim();
    if (trimmed) {
      const lead = raw.length - raw.trimStart().length;
      out.push({ sql: trimmed, start: start + lead, end: start + lead + trimmed.length });
    }
    start = nextStart;
  };

  const atLineStart = (pos: number) => {
    let p = pos - 1;
    while (p >= 0 && (text[p] === " " || text[p] === "\t")) p--;
    return p < 0 || text[p] === "\n";
  };

  while (i < n) {
    const ch = text[i];
    const next = text[i + 1];

    // Oracle PL/SQL block: run until a line containing only "/".
    if (dialect === "oracle" && i === start + (text.slice(start).length - text.slice(start).trimStart().length)) {
      if (isPlsqlStart(text.slice(i, i + 400))) {
        const re = /^[ \t]*\/[ \t]*$/gm;
        re.lastIndex = i;
        const m = re.exec(text);
        if (m) {
          push(m.index, m.index + m[0].length);
          i = m.index + m[0].length;
        } else {
          push(n, n);
          i = n;
        }
        continue;
      }
    }

    if (ch === "/" && next === "/" && (dialect === "mongodb" || dialect === "neo4j" || dialect === "janusgraph")) {
      const e = text.indexOf("\n", i);
      i = e < 0 ? n : e + 1;
      continue;
    }
    if (ch === "-" && next === "-" && dialect !== "mongodb" && dialect !== "janusgraph") {
      const e = text.indexOf("\n", i);
      i = e < 0 ? n : e + 1;
      continue;
    }
    if (ch === "/" && next === "*") {
      const e = text.indexOf("*/", i + 2);
      i = e < 0 ? n : e + 2;
      continue;
    }
    if (
      ch === "'" ||
      ch === '"' ||
      (ch === "`" && (dialect === "mysql" || dialect === "clickhouse" || dialect === "neo4j" || dialect === "mongodb")) ||
      (ch === "[" && dialect === "mssql")
    ) {
      const close = ch === "[" ? "]" : ch;
      i++;
      while (i < n) {
        if (text[i] === close) {
          if (text[i + 1] === close) {
            i += 2;
            continue;
          }
          break;
        }
        if (text[i] === "\\" && ["mysql", "clickhouse", "mongodb", "neo4j", "janusgraph"].includes(dialect) && close !== "`") i++;
        i++;
      }
      i++;
      continue;
    }
    if (ch === "$" && dialect === "postgres") {
      const m = /^\$[A-Za-z_]*\$/.exec(text.slice(i, i + 64));
      if (m) {
        const e = text.indexOf(m[0], i + m[0].length);
        i = e < 0 ? n : e + m[0].length;
        continue;
      }
    }
    if (dialect === "mssql" && (ch === "G" || ch === "g") && atLineStart(i)) {
      const m = /^go[ \t]*(\r?\n|$)/i.exec(text.slice(i, i + 8));
      if (m) {
        push(i, i + m[0].length);
        i += m[0].length;
        continue;
      }
    }
    if (ch === ";") {
      push(dialect === "oracle" ? i : i + 1, i + 1);
      i++;
      continue;
    }
    i++;
  }
  push(n, n);
  // Drop a trailing ";" we kept for non-Oracle dialects (drivers accept both, but keep display clean).
  return out.map((s) => (s.sql.endsWith(";") && !isPlsqlStart(s.sql) ? { ...s, sql: s.sql.slice(0, -1).trimEnd() } : s));
}

/** Redis: one command per non-empty line; `#` and `//` start comments. */
function splitLines(text: string): Stmt[] {
  const out: Stmt[] = [];
  let pos = 0;
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (t && !t.startsWith("#") && !t.startsWith("//")) {
      const lead = line.length - line.trimStart().length;
      out.push({ sql: t, start: pos + lead, end: pos + lead + t.length });
    }
    pos += line.length + 1;
  }
  return out;
}

const ES_METHOD = /^[ 	]*(GET|POST|PUT|DELETE|HEAD|PATCH)[ 	]+\S/i;

/**
 * Elasticsearch scripts mix Kibana console requests and SQL. A request is a `METHOD path` line
 * plus its JSON body; it runs until the next request line, or until a blank line followed by a
 * line that cannot continue JSON (SQL resumes there). Other text is SQL, split on `;`.
 */
function splitElastic(text: string): Stmt[] {
  const out: Stmt[] = [];
  const lines: { start: number; text: string }[] = [];
  for (let pos = 0; pos <= text.length; ) {
    const nl = text.indexOf("\n", pos);
    const end = nl < 0 ? text.length : nl;
    lines.push({ start: pos, text: text.slice(pos, end) });
    if (nl < 0) break;
    pos = nl + 1;
  }
  const pushRange = (from: number, to: number) => {
    const raw = text.slice(from, to);
    const sql = raw.trim();
    if (!sql) return;
    const lead = raw.length - raw.trimStart().length;
    out.push({ sql, start: from + lead, end: from + lead + sql.length });
  };
  const pushSql = (from: number, to: number) => {
    for (const s of splitStatements(text.slice(from, to), "sqlite")) out.push({ sql: s.sql, start: s.start + from, end: s.end + from });
  };
  let segStart = 0;
  let inRequest = false;
  let blank = false;
  for (const l of lines) {
    const t = l.text.trim();
    if (ES_METHOD.test(l.text)) {
      (inRequest ? pushRange : pushSql)(segStart, l.start);
      segStart = l.start;
      inRequest = true;
    } else if (inRequest && blank && t && !/^[{}[\]"\d,#/-]/.test(t)) {
      pushRange(segStart, l.start);
      segStart = l.start;
      inRequest = false;
    }
    blank = !t;
  }
  (inRequest ? pushRange : pushSql)(segStart, text.length);
  return out;
}

/** The statement under (or immediately before) the cursor offset. */
export function statementAt(text: string, offset: number, dialect: Dialect): Stmt | null {
  const stmts = splitStatements(text, dialect);
  if (!stmts.length) return null;
  let best: Stmt | null = null;
  for (const s of stmts) {
    if (offset >= s.start && offset <= s.end + 1) return s;
    if (s.start <= offset) best = s;
  }
  return best ?? stmts[0];
}

/** `:name` bind variables outside strings/comments (skips `::` casts and `:=`). */
export function findBindVars(sql: string, dialect?: Dialect): string[] {
  if (dialect && ["redis", "mongodb", "neo4j", "janusgraph", "elasticsearch"].includes(dialect)) return [];
  if (isPlsqlStart(sql) && leadingKeywords(sql).includes("trigger")) return [];
  const clean = sql
    .replace(/--[^\n]*|\/\*[\s\S]*?\*\//g, " ")
    .replace(/'(?:[^']|'')*'/g, "''")
    .replace(/"(?:[^"]|"")*"/g, '""');
  const names = new Set<string>();
  const re = /(^|[^:A-Za-z0-9_]):([A-Za-z_][A-Za-z0-9_]*)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(clean))) {
    const after = clean[m.index + m[0].length];
    if (after === "=") continue;
    names.add(m[2]);
  }
  return [...names];
}

export function substituteBinds(sql: string, values: Record<string, string>): string {
  return sql.replace(/(^|[^:A-Za-z0-9_]):([A-Za-z_][A-Za-z0-9_]*)(?!=)/g, (all, pre, name) => {
    if (!(name in values)) return all;
    const v = values[name];
    if (v.toUpperCase() === "NULL") return `${pre}NULL`;
    return /^-?\d+(\.\d+)?$/.test(v) ? `${pre}${v}` : `${pre}'${v.replace(/'/g, "''")}'`;
  });
}

export function quoteIdent(d: Dialect, name: string): string {
  const plain = /^[A-Za-z_][A-Za-z0-9_$#]*$/.test(name);
  const upperOk = d === "oracle" ? name === name.toUpperCase() : name === name.toLowerCase() || d !== "postgres";
  if (plain && upperOk) return name;
  if (d === "mysql" || d === "clickhouse" || d === "neo4j") return "`" + name.replace(/`/g, "``") + "`";
  if (d === "mssql") return "[" + name.replace(/]/g, "]]") + "]";
  return '"' + name.replace(/"/g, '""') + '"';
}

export function qualified(d: Dialect, schema: string, name: string): string {
  return schema ? `${quoteIdent(d, schema)}.${quoteIdent(d, name)}` : quoteIdent(d, name);
}

const redisQuote = (k: string) => (/^[^\s"'\\]+$/.test(k) ? k : `"${k.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`);

/** Statement that shows an object's data (kind matters for Redis / Neo4j). */
export function selectAllSql(d: Dialect, schema: string, name: string, limit: number, kind = "table"): string {
  if (d === "redis") {
    const k = redisQuote(name);
    switch (kind) {
      case "hash":
        return `HGETALL ${k}`;
      case "list":
        return `LRANGE ${k} 0 ${limit - 1}`;
      case "set":
        return `SMEMBERS ${k}`;
      case "zset":
        return `ZRANGE ${k} 0 ${limit - 1} WITHSCORES`;
      case "stream":
        return `XRANGE ${k} - + COUNT ${limit}`;
      default:
        return `GET ${k}`;
    }
  }
  if (d === "mongodb") return `db.getSiblingDB(${JSON.stringify(schema)}).getCollection(${JSON.stringify(name)}).find({}).limit(${limit})`;
  if (d === "neo4j") {
    const n = "`" + name.replace(/`/g, "``") + "`";
    return kind === "relationship" ? `MATCH (a)-[r:${n}]->(b) RETURN a, r, b LIMIT ${limit}` : `MATCH (n:${n}) RETURN n LIMIT ${limit}`;
  }
  if (d === "elasticsearch") return `GET ${name}/_search\n{\n  "size": ${limit},\n  "query": { "match_all": {} }\n}`;
  if (d === "janusgraph") {
    const l = `'${name.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
    return kind === "relationship"
      ? `g.E().hasLabel(${l}).limit(${limit}).project('a', 'r', 'b').by(outV()).by().by(inV())`
      : `g.V().hasLabel(${l}).limit(${limit})`;
  }
  const t = qualified(d, schema, name);
  switch (d) {
    case "mssql":
      return `SELECT TOP ${limit} * FROM ${t}`;
    case "oracle":
      return `SELECT * FROM ${t} FETCH FIRST ${limit} ROWS ONLY`;
    default:
      return `SELECT * FROM ${t} LIMIT ${limit}`;
  }
}

export function literal(d: Dialect, v: CellValue): string {
  if (v === null) return "NULL";
  if (typeof v === "number") return String(v);
  if (typeof v === "boolean") return d === "oracle" || d === "mssql" || d === "sqlite" ? (v ? "1" : "0") : v ? "TRUE" : "FALSE";
  return `'${String(v).replace(/'/g, "''")}'`;
}

export function explainSql(d: Dialect, sql: string): string {
  switch (d) {
    case "postgres":
      return `EXPLAIN ${sql}`;
    case "mysql":
      return `EXPLAIN ${sql}`;
    case "sqlite":
      return `EXPLAIN QUERY PLAN ${sql}`;
    case "oracle":
      return `EXPLAIN PLAN FOR ${sql}`;
    case "clickhouse":
      return `EXPLAIN ${sql}`;
    case "neo4j":
      return `EXPLAIN ${sql}`;
    default:
      return sql;
  }
}
