import { api, type Dialect, type SqlAnalysis } from "../../lib/ipc";

/** Identifier inserted at the cursor so half-typed statements (`SELECT x. FROM t x`) still parse. */
export const PROBE = "__dbf_c";

type Derived = SqlAnalysis["derived"][number];

const MAX_CACHED = 400;
const cache = new Map<string, SqlAnalysis>();

/** Analyses statements through the backend parser; results are cached per (dialect, text). */
export async function analyze(dialect: Dialect, statements: string[]): Promise<SqlAnalysis[]> {
  const key = (s: string) => `${dialect}\u0001${s}`;
  const missing = [...new Set(statements.filter((s) => !cache.has(key(s))))];
  if (missing.length) {
    const res = await api.sqlAnalyze(dialect, missing);
    missing.forEach((s, i) => cache.set(key(s), res[i]));
    while (cache.size > MAX_CACHED) cache.delete(cache.keys().next().value!);
  }
  return statements.map((s) => cache.get(key(s))!);
}

export async function analyzeOne(dialect: Dialect, statement: string): Promise<SqlAnalysis | null> {
  try {
    const [a] = await analyze(dialect, [statement]);
    return a.error ? null : a;
  } catch {
    return null;
  }
}

const lc = (s: string) => s.toLowerCase();

/**
 * alias / table short name (lower-case) → table as written, for real tables only (not CTEs).
 * An alias bound to different tables in different scopes maps to null (ambiguous).
 */
export function tableAliases(a: SqlAnalysis): Map<string, string | null> {
  const m = new Map<string, string | null>();
  const bind = (k: string, v: string) => {
    const prev = m.get(k);
    m.set(k, prev === undefined || (prev !== null && lc(prev) === lc(v)) ? v : null);
  };
  for (const t of a.tables) {
    if (t.cte || t.name.includes(PROBE)) continue;
    bind(lc(t.name.split(".").pop()!), t.name);
    if (t.alias) bind(lc(t.alias), t.name);
  }
  return m;
}

/** CTE or subquery that `qualifier` names: a subquery alias, a CTE name, or an alias of a CTE reference. */
export function derivedFor(a: SqlAnalysis, qualifier: string): Derived | undefined {
  const q = lc(qualifier);
  const direct = a.derived.find((d) => lc(d.name) === q);
  if (direct) return direct;
  const ref = a.tables.find((t) => t.cte && t.alias && lc(t.alias) === q);
  return ref ? a.derived.find((d) => d.kind === "cte" && lc(d.name) === lc(ref.name)) : undefined;
}

/** Converts a statement-relative (line, column) to an absolute offset in the editor text. */
export function offsetIn(stmtText: string, stmtStart: number, line: number, column: number): number {
  let off = 0;
  for (let l = 1; l < line; l++) {
    const nl = stmtText.indexOf("\n", off);
    if (nl < 0) break;
    off = nl + 1;
  }
  return stmtStart + off + Math.max(0, column - 1);
}
