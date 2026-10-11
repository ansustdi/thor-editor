import { confirmAsk } from "../../lib/confirm";
import clsx from "clsx";
import { CheckCheck, ChevronDown, PlugZap, Plus, RefreshCw, Undo2, Unplug, Activity, MonitorCog } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { describeTarget } from "../../lib/engines";
import { api, errorText } from "../../lib/ipc";
import { useApp, uid, type Connection } from "../../store/app";

interface Toast {
  id: number;
  ok: boolean;
  text: string;
}

const label = (c: Connection) => c.profile.name || `${c.profile.username || "os"}@${describeTarget(c.profile)}`;

/** Toad-style Session menu: test/reconnect connections, commit/rollback, end connections. */
export function SessionMenu({ onDisconnect }: { onDisconnect: (key: string) => Promise<void> }) {
  const { connections, activeConn, setConnStatus, setLoginOpen, tabs, openTab, setActiveTab } = useApp();

  const openBrowser = () => {
    if (!active) return;
    const existing = tabs.find((t) => t.connKey === active.key && t.kind === "sessions");
    if (existing) setActiveTab(active.key, existing.id);
    else openTab({ id: uid(), connKey: active.key, kind: "sessions", title: "Sessions" });
  };
  const [open, setOpen] = useState(false);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const seq = useRef(0);
  const active = connections.find((c) => c.key === activeConn);
  const lost = connections.filter((c) => c.status === "lost");

  useEffect(() => {
    if (!open) return;
    const h = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [open]);

  const toast = (text: string, ok = true) => {
    const id = ++seq.current;
    setToasts((t) => [...t, { id, ok, text }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), ok ? 4000 : 9000);
  };

  const test = async (c: Connection, quiet = false) => {
    setConnStatus(c.key, "checking");
    try {
      const r = await api.testSession(c.info.sessionId);
      if (r.alive) {
        setConnStatus(c.key, "ok", r.reconnected ? "Reconnected" : `OK (${r.latencyMs} ms)`);
        if (r.reconnected) toast(`${label(c)}: connection was lost and has been re-established.`, true);
        else if (!quiet) toast(`${label(c)}: connection OK (${r.latencyMs} ms)`, true);
      } else {
        setConnStatus(c.key, "lost", r.error ?? "unreachable");
        toast(`${label(c)}: ${r.error?.split("\n")[0] ?? "connection lost"}`, false);
      }
    } catch (e) {
      setConnStatus(c.key, "lost", errorText(e));
      toast(`${label(c)}: ${errorText(e)}`, false);
    }
  };

  const txn = async (what: "commit" | "rollback") => {
    if (!active) return;
    try {
      await (what === "commit" ? api.commit(active.info.sessionId) : api.rollback(active.info.sessionId));
      toast(`${label(active)}: ${what === "commit" ? "committed" : "rolled back"}`);
    } catch (e) {
      toast(errorText(e), false);
    }
  };

  const run = (fn: () => void | Promise<void>) => () => {
    setOpen(false);
    fn();
  };

  const items: ({ label: string; icon: React.ReactNode; onClick: () => void; disabled?: boolean; hint?: string } | "sep")[] = [
    { label: "New Connection…", icon: <Plus size={14} />, onClick: run(() => setLoginOpen(true)), hint: "Ctrl+Shift+N" },
    { label: "End Connection", icon: <Unplug size={14} />, onClick: run(() => active && onDisconnect(active.key)), disabled: !active },
    {
      label: "End All Connections",
      icon: <Unplug size={14} className="text-danger" />,
      onClick: run(async () => {
        if (!(await confirmAsk(`Disconnect all ${connections.length} connection(s)?`, { danger: true }))) return;
        for (const c of [...connections]) await onDisconnect(c.key);
      }),
      disabled: !connections.length,
    },
    "sep",
    { label: "Test Connection (Reconnect)", icon: <PlugZap size={14} />, onClick: run(() => active && test(active)), disabled: !active },
    {
      label: "Test All Connections (Reconnect)",
      icon: <Activity size={14} />,
      onClick: run(() => Promise.all(connections.map((c) => test(c))).then(() => undefined)),
      disabled: !connections.length,
    },
    {
      label: `Reconnect Lost Connections${lost.length ? ` (${lost.length})` : ""}`,
      icon: <RefreshCw size={14} />,
      onClick: run(() => Promise.all(lost.map((c) => test(c))).then(() => undefined)),
      disabled: !lost.length,
    },
    "sep",
    {
      label: "Session Browser",
      icon: <MonitorCog size={14} />,
      onClick: run(openBrowser),
      disabled: !active || !["oracle", "postgres", "mysql", "mssql", "clickhouse"].includes(active.info.dialect),
    },
    "sep",
    { label: "Commit", icon: <CheckCheck size={14} className="text-ok" />, onClick: run(() => txn("commit")), disabled: !active },
    { label: "Rollback", icon: <Undo2 size={14} className="text-danger" />, onClick: run(() => txn("rollback")), disabled: !active },
  ];

  return (
    <>
      <div className="relative">
        <button
          onClick={() => setOpen((v) => !v)}
          className={clsx("inline-flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium hover:bg-hover", open && "bg-hover")}
        >
          Session
          {lost.length > 0 && <span className="h-1.5 w-1.5 rounded-full bg-danger" />}
          <ChevronDown size={12} />
        </button>
        {open && (
          <>
            <div className="fixed inset-0 z-40" onMouseDown={() => setOpen(false)} />
            <div className="absolute right-0 top-8 z-50 w-72 rounded-md border border-line bg-panel py-1 text-[13px] shadow-xl">
              {items.map((it, i) =>
                it === "sep" ? (
                  <div key={i} className="my-1 border-t border-line" />
                ) : (
                  <button
                    key={it.label}
                    disabled={it.disabled}
                    onClick={it.onClick}
                    className="flex w-full items-center gap-2.5 px-3 py-1.5 text-left hover:bg-hover disabled:pointer-events-none disabled:opacity-40"
                  >
                    <span className="w-4 text-muted">{it.icon}</span>
                    <span className="flex-1">{it.label}</span>
                    {it.hint && <span className="text-[11px] text-muted">{it.hint}</span>}
                  </button>
                ),
              )}
            </div>
          </>
        )}
      </div>
      <div className="pointer-events-none fixed bottom-8 right-4 z-[60] flex w-96 flex-col gap-2">
        {toasts.map((t) => (
          <div
            key={t.id}
            className={clsx(
              "pointer-events-auto rounded-md border px-3 py-2 text-xs shadow-lg select-text",
              t.ok ? "border-ok/40 bg-panel text-fg" : "border-danger/40 bg-panel text-danger",
            )}
          >
            {t.text}
          </div>
        ))}
      </div>
    </>
  );
}

export function StatusDot({ c }: { c: Connection }) {
  const cls = c.status === "lost" ? "bg-danger" : c.status === "checking" ? "animate-pulse bg-warn" : "bg-ok";
  const title = c.status === "lost" ? `Connection lost: ${c.statusText ?? ""}` : c.status === "checking" ? "Checking…" : c.statusText ?? "Connected";
  return <span className={clsx("h-2 w-2 shrink-0 rounded-full", cls)} title={title} />;
}
