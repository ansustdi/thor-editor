import { confirmAsk } from "../../lib/confirm";
import clsx from "clsx";
import { Ban, Copy, FileInput, Lock, RefreshCw, Search, Skull, Timer } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Checkbox, Input, Select, Spinner } from "../../components/ui";
import { api, errorText, type DbSession, type Grid, type SessionDetail } from "../../lib/ipc";
import { useApp, uid, type Connection } from "../../store/app";

const fmtSecs = (s: number | null) => {
  if (s === null || s === undefined) return "";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
};

const isActive = (s: DbSession) => /^(active|running|query|executing)/i.test(s.status) || s.status.toLowerCase() === "running";

function GridTable({ g, empty }: { g: Grid; empty: string }) {
  if (!g.rows.length) return <div className="px-3 py-2 text-xs text-muted">{empty}</div>;
  const pctCol = g.columns.findIndex((c) => c.toLowerCase() === "pct");
  return (
    <table className="w-full text-xs">
      <thead className="bg-panel-2 text-left text-muted">
        <tr>
          {g.columns.map((c) => (
            <th key={c} className="px-2 py-1 font-medium">
              {c}
            </th>
          ))}
        </tr>
      </thead>
      <tbody className="font-mono select-text">
        {g.rows.map((r, i) => (
          <tr key={i} className="border-b border-line/50">
            {r.map((v, j) => (
              <td key={j} className="px-2 py-1">
                {j === pctCol && v ? (
                  <span className="inline-flex items-center gap-2">
                    <span className="inline-block h-1.5 w-24 overflow-hidden rounded bg-line">
                      <span className="block h-full bg-accent" style={{ width: `${Math.min(100, Number(v))}%` }} />
                    </span>
                    {v}%
                  </span>
                ) : (
                  v
                )}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** Toad-style session browser: who is connected, what runs, who blocks whom; cancel / kill. */
export function SessionBrowser({ conn, visible }: { conn: Connection; visible: boolean }) {
  const { openTab } = useApp();
  const [list, setList] = useState<DbSession[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");
  const [activeOnly, setActiveOnly] = useState(false);
  const [userOnly, setUserOnly] = useState(false);
  const [auto, setAuto] = useState(0);
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const busy = useRef(false);
  const sid = conn.info.sessionId;

  const refresh = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    setLoading(true);
    try {
      setList(await api.sessionList(sid));
      setError("");
    } catch (e) {
      setError(errorText(e));
    } finally {
      busy.current = false;
      setLoading(false);
    }
  }, [sid]);

  useEffect(() => {
    if (visible) refresh();
  }, [visible, refresh]);

  useEffect(() => {
    if (!auto || !visible) return;
    const h = setInterval(refresh, auto * 1000);
    return () => clearInterval(h);
  }, [auto, visible, refresh]);

  const sel = list.find((s) => s.id === selected) ?? null;

  useEffect(() => {
    if (!sel) {
      setDetail(null);
      return;
    }
    let live = true;
    api
      .sessionDetail(sid, sel.id, sel.sqlId)
      .then((d) => live && setDetail(d))
      .catch((e) => live && setDetail({ sqlFull: "", locks: { columns: ["error"], rows: [[errorText(e)]] }, longOps: { columns: [], rows: [] } }));
    return () => {
      live = false;
    };
    // refresh detail when the selected session's SQL / status changes
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sel?.id, sel?.sqlId, sel?.status, sid]);

  // sid → number of sessions it blocks
  const blocking = useMemo(() => {
    const m = new Map<string, number>();
    for (const s of list) for (const b of s.blockedBy.split(",").filter(Boolean)) m.set(b.trim(), (m.get(b.trim()) ?? 0) + 1);
    return m;
  }, [list]);

  const myUser = conn.profile.username.toLowerCase();
  const shown = list.filter((s) => {
    if (activeOnly && !isActive(s) && !s.blockedBy) return false;
    if (userOnly && s.user.toLowerCase() !== myUser) return false;
    const q = filter.trim().toLowerCase();
    return !q || [s.sid, s.user, s.machine, s.program, s.osUser, s.schema, s.sqlText, s.status].some((v) => v.toLowerCase().includes(q));
  });

  const kill = async (s: DbSession, cancelOnly: boolean) => {
    const what = cancelOnly ? "Cancel the running statement of" : "KILL";
    const self = s.isSelf ? "\n\nThis is YOUR editor session." : "";
    if (!(await confirmAsk(`${what} session ${s.sid} (${s.user} · ${s.machine} · ${s.program})?${self}`))) return;
    try {
      await api.sessionKill(sid, s.id, cancelOnly);
      setMsg({ ok: true, text: `${cancelOnly ? "Cancel sent to" : "Killed"} session ${s.sid}.` });
      setTimeout(refresh, 600);
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
    }
  };

  const activeCount = list.filter(isActive).length;
  const blockedCount = list.filter((s) => s.blockedBy).length;

  return (
    <div className="flex h-full min-h-0 flex-col bg-panel">
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-3 py-1.5">
        <span className="font-semibold">Session Browser</span>
        <span className="text-xs text-muted">
          {list.length} sessions · {activeCount} active
          {blockedCount > 0 && <span className="font-semibold text-danger"> · {blockedCount} blocked</span>}
        </span>
        <div className="relative ml-2 w-56">
          <Search size={13} className="absolute left-2 top-2 text-muted" />
          <Input className="h-7 pl-7 text-xs" placeholder="Filter (user, machine, SQL…)" value={filter} onChange={(e) => setFilter(e.target.value)} />
        </div>
        <Checkbox label="Active / blocked only" checked={activeOnly} onChange={setActiveOnly} />
        <Checkbox label={`Only ${conn.profile.username || "my user"}`} checked={userOnly} onChange={setUserOnly} />
        <div className="flex-1" />
        {loading && <Spinner size={12} />}
        <Select className="h-7 w-32 text-xs" value={String(auto)} onChange={(e) => setAuto(Number(e.target.value))} title="Auto refresh">
          <option value="0">Auto: off</option>
          <option value="3">Every 3 s</option>
          <option value="5">Every 5 s</option>
          <option value="10">Every 10 s</option>
          <option value="30">Every 30 s</option>
        </Select>
        <Button size="sm" variant="ghost" onClick={refresh}>
          <RefreshCw size={13} /> Refresh
        </Button>
      </div>
      {error && <div className="m-2 whitespace-pre-wrap rounded bg-danger/10 p-2 text-xs text-danger select-text">{error}</div>}
      {msg && (
        <div className={clsx("mx-2 mt-2 rounded px-3 py-1.5 text-xs", msg.ok ? "bg-ok/10 text-ok" : "bg-danger/10 text-danger")}>{msg.text}</div>
      )}

      <div className="min-h-0 flex-1 overflow-auto">
        <table className="w-full text-[12.5px]">
          <thead className="sticky top-0 z-10 bg-panel-2 text-left text-xs text-muted">
            <tr>
              {["SID", "User", "Status", "Machine", "Program", "Schema", "Time", "Wait", "Blocked by", "SQL"].map((h) => (
                <th key={h} className="border-b border-line px-2 py-1.5 font-medium">
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((s) => {
              const blocks = blocking.get(s.sid) ?? 0;
              return (
                <tr
                  key={s.id}
                  onClick={() => setSelected(s.id)}
                  className={clsx(
                    "cursor-default border-b border-line/50",
                    selected === s.id ? "bg-accent-soft" : s.blockedBy ? "bg-danger/10" : blocks ? "bg-warn/10" : "hover:bg-hover",
                  )}
                >
                  <td className="whitespace-nowrap px-2 py-1 font-mono">
                    {s.sid}
                    {s.isSelf && <span className="ml-1 rounded bg-accent px-1 text-[10px] font-semibold text-white">you</span>}
                    {blocks > 0 && (
                      <span className="ml-1 inline-flex items-center gap-0.5 rounded bg-warn px-1 text-[10px] font-semibold text-white" title={`Blocking ${blocks} session(s)`}>
                        <Lock size={9} /> {blocks}
                      </span>
                    )}
                  </td>
                  <td className="px-2 py-1 font-medium">{s.user}</td>
                  <td className={clsx("px-2 py-1", isActive(s) ? "font-semibold text-ok" : "text-muted")}>{s.status}</td>
                  <td className="max-w-40 truncate px-2 py-1" title={s.machine}>{s.machine}</td>
                  <td className="max-w-40 truncate px-2 py-1" title={s.program}>{s.program}</td>
                  <td className="px-2 py-1">{s.schema}</td>
                  <td className="whitespace-nowrap px-2 py-1 text-right font-mono">{fmtSecs(s.seconds)}</td>
                  <td className="max-w-56 truncate px-2 py-1 text-xs" title={s.wait}>{s.wait}</td>
                  <td className="px-2 py-1 font-mono font-semibold text-danger">{s.blockedBy}</td>
                  <td className="max-w-96 truncate px-2 py-1 font-mono text-xs" title={s.sqlText}>{s.sqlText}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
        {!error && !loading && shown.length === 0 && <div className="p-6 text-center text-muted">No sessions match.</div>}
      </div>

      {sel && (
        <div className="flex h-72 shrink-0 flex-col border-t-2 border-line">
          <div className="flex items-center gap-2 border-b border-line bg-panel-2 px-3 py-1.5 text-xs">
            <b className="font-mono">{sel.sid}</b>
            <span>{sel.user}</span>
            <span className="text-muted">
              {sel.osUser && `${sel.osUser} @ `}
              {sel.machine} · {sel.program} · logon {sel.logonTime}
            </span>
            {sel.blockedBy && (
              <button className="font-semibold text-danger underline" onClick={() => setSelected(list.find((x) => x.sid === sel.blockedBy.split(",")[0])?.id ?? null)}>
                blocked by {sel.blockedBy}
              </button>
            )}
            <div className="flex-1" />
            <Button size="sm" variant="ghost" disabled={!detail?.sqlFull && !sel.sqlText} onClick={() => navigator.clipboard.writeText(detail?.sqlFull || sel.sqlText)}>
              <Copy size={13} /> Copy SQL
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={!detail?.sqlFull && !sel.sqlText}
              onClick={() => openTab({ id: uid(), connKey: conn.key, kind: "sql", title: `SID ${sel.sid}`, sql: detail?.sqlFull || sel.sqlText })}
            >
              <FileInput size={13} /> Open SQL
            </Button>
            <Button size="sm" onClick={() => kill(sel, true)} title="Stop the running statement, keep the session">
              <Ban size={13} /> Cancel SQL
            </Button>
            <Button size="sm" variant="danger" onClick={() => kill(sel, false)}>
              <Skull size={13} /> Kill session
            </Button>
          </div>
          <div className="grid min-h-0 flex-1 grid-cols-2">
            <div className="flex min-h-0 flex-col border-r border-line">
              <div className="px-3 py-1 text-[11px] font-semibold uppercase text-muted">Current SQL{sel.wait && <span className="ml-2 normal-case text-warn">· {sel.wait}</span>}</div>
              <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap px-3 pb-2 font-mono text-xs select-text">
                {detail ? detail.sqlFull || sel.sqlText || "(idle — no SQL running)" : "…"}
              </pre>
            </div>
            <div className="min-h-0 overflow-auto">
              <div className="flex items-center gap-1 px-3 py-1 text-[11px] font-semibold uppercase text-muted">
                <Lock size={11} /> Locks held
              </div>
              {detail ? <GridTable g={detail.locks} empty="No locks." /> : <div className="px-3 text-xs text-muted">…</div>}
              <div className="mt-2 flex items-center gap-1 px-3 py-1 text-[11px] font-semibold uppercase text-muted">
                <Timer size={11} /> Long operations
              </div>
              {detail ? <GridTable g={detail.longOps} empty="None running." /> : null}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
