import clsx from "clsx";
import { ArrowLeftRight } from "lucide-react";
import { useEffect, useState } from "react";
import { Button, Field, Input, Modal, Select, Spinner } from "../../components/ui";
import { describeTarget } from "../../lib/engines";
import { displayValue } from "../../lib/format";
import { api, errorText, type CompareResult, type QueryResult } from "../../lib/ipc";
import { useApp } from "../../store/app";

const MAX_ROWS = 200_000;

interface Side {
  conn: string;
  sql: string;
}

export function CompareDialog({ onClose }: { onClose: () => void }) {
  const { connections, activeConn } = useApp();
  const first = activeConn ?? connections[0]?.key ?? "";
  const [a, setA] = useState<Side>({ conn: first, sql: "" });
  const [b, setB] = useState<Side>({ conn: first, sql: "" });
  const [keys, setKeys] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [res, setRes] = useState<CompareResult | null>(null);
  const [filter, setFilter] = useState<"all" | "changed" | "only_a" | "only_b">("all");
  const [held, setHeld] = useState<string[]>([]);

  useEffect(() => () => held.forEach((id) => api.releaseResult(id).catch(() => {})), [held]);

  const runSide = async (s: Side, label: string): Promise<QueryResult> => {
    const c = connections.find((x) => x.key === s.conn);
    if (!c) throw new Error(`Choose a connection for ${label}`);
    setBusy(`Running ${label}…`);
    const r = await api.execute(c.info.sessionId, s.sql.trim().replace(/;\s*$/, ""), MAX_ROWS);
    if (!r.columns.length) throw new Error(`${label} returned no result set`);
    return r;
  };

  const run = async () => {
    setError("");
    setRes(null);
    try {
      const ra = await runSide(a, "A");
      const rb = await runSide(b, "B");
      setHeld([ra.resultId, rb.resultId]);
      if (ra.truncated || rb.truncated) setError(`Only the first ${MAX_ROWS.toLocaleString()} rows of each side were compared.`);
      const keyList = keys.split(",").map((k) => k.trim()).filter(Boolean);
      if (!keyList.length && ra.columns[0]) setKeys(ra.columns[0].name);
      setBusy("Comparing…");
      setRes(await api.compareResults(ra.resultId, rb.resultId, keyList.length ? keyList : [ra.columns[0].name]));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy("");
    }
  };

  const rows = res?.rows.filter((r) => filter === "all" || r.status === filter) ?? [];

  const sideEditor = (s: Side, set: (s: Side) => void, label: string) => (
    <div className="flex min-w-0 flex-1 flex-col gap-1.5">
      <div className="flex items-center gap-2">
        <span className="rounded bg-accent-soft px-1.5 text-xs font-bold text-accent">{label}</span>
        <Select value={s.conn} onChange={(e) => set({ ...s, conn: e.target.value })}>
          {connections.map((c) => (
            <option key={c.key} value={c.key}>
              {c.profile.name || `${c.profile.username}@${describeTarget(c.profile)}`}
            </option>
          ))}
        </Select>
      </div>
      <textarea
        className="h-24 w-full resize-none rounded-md border border-line bg-panel p-2 font-mono text-xs outline-none focus:border-accent"
        placeholder="SELECT * FROM customers"
        value={s.sql}
        onChange={(e) => set({ ...s, sql: e.target.value })}
      />
    </div>
  );

  return (
    <Modal
      title="Compare data"
      onClose={onClose}
      width={1180}
      footer={
        <>
          {busy && (
            <span className="mr-auto inline-flex items-center gap-2 text-xs text-muted">
              <Spinner size={12} /> {busy}
            </span>
          )}
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={run} disabled={!!busy || !a.sql.trim() || !b.sql.trim()}>
            Compare
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="flex items-start gap-3">
          {sideEditor(a, setA, "A")}
          <button className="mt-10 rounded p-1 text-muted hover:bg-hover" title="Swap" onClick={() => { setA(b); setB(a); }}>
            <ArrowLeftRight size={16} />
          </button>
          {sideEditor(b, setB, "B")}
        </div>
        <Field label="Key columns (comma separated; default: first column)">
          <Input value={keys} onChange={(e) => setKeys(e.target.value)} placeholder="id" />
        </Field>
        {error && <div className="rounded bg-warn/10 px-3 py-2 text-xs text-warn select-text">{error}</div>}
        {res && (
          <>
            <div className="flex flex-wrap items-center gap-2 text-xs">
              {(
                [
                  ["all", `All differences (${(res.changed + res.onlyA + res.onlyB).toLocaleString()})`, ""],
                  ["changed", `Changed ${res.changed.toLocaleString()}`, "text-warn"],
                  ["only_a", `Only in A ${res.onlyA.toLocaleString()}`, "text-danger"],
                  ["only_b", `Only in B ${res.onlyB.toLocaleString()}`, "text-ok"],
                ] as const
              ).map(([k, label, cls]) => (
                <button
                  key={k}
                  onClick={() => setFilter(k)}
                  className={clsx("rounded-md border px-2 py-1 font-medium", filter === k ? "border-accent bg-accent-soft" : "border-line hover:bg-hover", cls)}
                >
                  {label}
                </button>
              ))}
              <span className="text-muted">
                Identical: {res.same.toLocaleString()} · key: {res.keyColumns.join(", ")}
                {res.duplicateKeys > 0 && <span className="text-warn"> · {res.duplicateKeys} duplicate keys</span>}
                {res.truncated && <span className="text-warn"> · first 5,000 differences shown</span>}
              </span>
            </div>
            <div className="max-h-[420px] overflow-auto rounded-md border border-line">
              <table className="w-full text-xs">
                <thead className="sticky top-0 bg-panel-2 text-left">
                  <tr>
                    <th className="w-20 border-b border-line px-2 py-1.5">Status</th>
                    <th className="w-6 border-b border-line" />
                    {res.columns.map((c) => (
                      <th key={c} className={clsx("border-b border-line px-2 py-1.5", res.keyColumns.includes(c) && "text-accent")}>
                        {c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody className="font-mono select-text">
                  {rows.map((r, i) =>
                    (r.status === "changed" ? (["a", "b"] as const) : [r.a ? ("a" as const) : ("b" as const)]).map((side) => {
                      const vals = side === "a" ? r.a! : r.b!;
                      return (
                        <tr key={`${i}${side}`} className={clsx(side === "b" && r.status === "changed" ? "border-b border-line" : side === "a" && r.status === "changed" ? "" : "border-b border-line/60")}>
                          {side === "a" || r.status !== "changed" ? (
                            <td rowSpan={r.status === "changed" ? 2 : 1} className={clsx("px-2 py-0.5 font-sans font-medium", r.status === "changed" ? "text-warn" : r.status === "only_a" ? "text-danger" : "text-ok")}>
                              {r.status === "changed" ? "changed" : r.status === "only_a" ? "only A" : "only B"}
                            </td>
                          ) : null}
                          <td className="px-1 text-muted">{side.toUpperCase()}</td>
                          {vals.map((v, j) => (
                            <td key={j} className={clsx("max-w-64 truncate px-2 py-0.5", r.changed.includes(j) && (side === "a" ? "bg-danger/10" : "bg-ok/10"), v === null && "text-muted")}>
                              {displayValue(v)}
                            </td>
                          ))}
                        </tr>
                      );
                    }),
                  )}
                </tbody>
              </table>
              {rows.length === 0 && <div className="p-6 text-center text-muted">No differences{filter !== "all" ? " in this category" : ""}.</div>}
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}
