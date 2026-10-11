import { confirmAsk } from "./lib/confirm";
import clsx from "clsx";
import { Database, GitCompare, Moon, Plug, Settings as SettingsIcon, Sun, Unplug, X } from "lucide-react";
import { useEffect, useState } from "react";
import { CompareDialog } from "./features/compare/CompareDialog";
import { SessionMenu, StatusDot } from "./features/session/SessionMenu";
import { Button, Checkbox, EngineBadge, Field, IconButton, Input, Modal, Select } from "./components/ui";
import { LoginDialog } from "./features/login/LoginDialog";
import { Workspace } from "./features/workspace/Workspace";
import { describeTarget, engineInfo } from "./lib/engines";
import { api, onSessionReconnected } from "./lib/ipc";
import { useApplyTheme, useThemeColors } from "./lib/theme";
import { useApp, uid } from "./store/app";

export default function App() {
  const s = useApp();
  const colors = useThemeColors();
  const [comparing, setComparing] = useState(false);
  useApplyTheme(s.settings.theme);

  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.key.toLowerCase() === "t" && s.activeConn) {
        e.preventDefault();
        s.openTab({ id: uid(), connKey: s.activeConn, kind: "sql", title: "SQL", sql: "" });
      }
      if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === "n") {
        e.preventDefault();
        s.setLoginOpen(true);
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [s]);

  const disconnect = async (key: string) => {
    const c = s.connections.find((x) => x.key === key);
    if (!c) return;
    if (!c.autocommit && !(await confirmAsk(`Disconnect ${c.profile.username}? Uncommitted changes will be rolled back.`))) return;
    await api.rollback(c.info.sessionId).catch(() => {});
    await api.disconnect(c.info.sessionId).catch(() => {});
    s.closeConnection(key);
  };

  const active = s.connections.find((c) => c.key === s.activeConn);

  // Keep-alive results from the backend drive the green/red status dots.
  useEffect(() => {
    const un = onSessionReconnected((e) => {
      const c = useApp.getState().connections.find((x) => x.info.sessionId === e.sessionId);
      if (c) useApp.getState().setConnStatus(c.key, e.ok ? "ok" : "lost", e.ok ? "Reconnected automatically" : e.reason);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  return (
    <div className="flex h-full flex-col">
      {/* Top bar: connection tabs */}
      <div className="flex h-10 shrink-0 items-center gap-1 border-b border-line bg-panel px-2">
        <div className="mr-2 flex items-center gap-1.5 font-semibold">
          <Database size={17} className="text-accent" /> Thor Editor
        </div>
        <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
          {s.connections.map((c) => {
            const e = engineInfo(c.profile.engine);
            return (
              <div
                key={c.key}
                onClick={() => s.setActiveConn(c.key)}
                className={clsx(
                  "group flex h-8 shrink-0 cursor-default items-center gap-2 rounded-md border px-2 text-xs",
                  s.activeConn === c.key ? "border-line bg-panel-2 font-semibold" : "border-transparent text-muted hover:bg-hover",
                )}
                style={c.profile.color ? { boxShadow: `inset 0 -3px 0 ${c.profile.color}` } : undefined}
                title={`${c.info.version}\n${describeTarget(c.profile)}`}
              >
                <StatusDot c={c} />
                <EngineBadge short={e.short} color={e.color} size={18} />
                <span className="max-w-52 truncate">
                  {c.profile.name || `${c.profile.username || "os"}@${describeTarget(c.profile)}`}
                </span>
                <button
                  title="Disconnect"
                  className="rounded opacity-40 hover:bg-hover hover:opacity-100"
                  onClick={(ev) => {
                    ev.stopPropagation();
                    disconnect(c.key);
                  }}
                >
                  <X size={13} />
                </button>
              </div>
            );
          })}
        </div>
        <SessionMenu onDisconnect={disconnect} />
        <Button size="sm" variant="primary" onClick={() => s.setLoginOpen(true)} title="New connection (Ctrl+Shift+N)">
          <Plug size={13} /> Connect
        </Button>
        {s.connections.length > 0 && (
          <IconButton title="Compare data between queries / databases" onClick={() => setComparing(true)}>
            <GitCompare size={15} />
          </IconButton>
        )}
        {active && (
          <IconButton title="Disconnect current" onClick={() => disconnect(active.key)}>
            <Unplug size={15} />
          </IconButton>
        )}
        <IconButton
          title="Toggle theme"
          onClick={() => s.updateSettings({ theme: colors.dark ? "light" : "dark" })}
        >
          {colors.dark ? <Sun size={15} /> : <Moon size={15} />}
        </IconButton>
        <IconButton title="Settings" onClick={() => s.setSettingsOpen(true)}>
          <SettingsIcon size={15} />
        </IconButton>
      </div>

      {s.connections.map((c) => (
        <Workspace key={c.key} conn={c} visible={c.key === s.activeConn} />
      ))}
      {s.connections.length === 0 && !s.loginOpen && (
        <div className="flex flex-1 items-center justify-center">
          <Button variant="primary" onClick={() => s.setLoginOpen(true)}>
            <Plug size={14} /> Connect to a database
          </Button>
        </div>
      )}

      {/* Status bar */}
      <div className="flex h-6 shrink-0 items-center gap-4 border-t border-line bg-panel-2 px-3 text-[11px] text-muted">
        {active ? (
          <>
            <span>{engineInfo(active.profile.engine).label}</span>
            <span className="truncate">{active.info.version.split("\n")[0]}</span>
            <span>schema: {active.info.currentSchema || "—"}</span>
            <span className={active.autocommit ? "" : "text-warn"}>auto-commit {active.autocommit ? "on" : "off"}</span>
          </>
        ) : (
          <span>Not connected</span>
        )}
        <span className="ml-auto">{s.connections.length} connection(s)</span>
      </div>

      {s.loginOpen && <LoginDialog />}
      {s.settingsOpen && <SettingsDialog />}
      {comparing && <CompareDialog onClose={() => setComparing(false)} />}
    </div>
  );
}

function SettingsDialog() {
  const { settings, updateSettings, setSettingsOpen, clearHistory, history, snippets, deleteSnippet } = useApp();
  return (
    <Modal title="Settings" onClose={() => setSettingsOpen(false)} width={520} footer={<Button onClick={() => setSettingsOpen(false)}>Close</Button>}>
      <div className="space-y-4">
        <div className="grid grid-cols-3 gap-3">
          <Field label="Theme">
            <Select value={settings.theme} onChange={(e) => updateSettings({ theme: e.target.value as "light" | "dark" | "system" })}>
              <option value="system">System</option>
              <option value="light">Light</option>
              <option value="dark">Dark</option>
            </Select>
          </Field>
          <Field label="Fetch size (rows)">
            <Input
              type="number"
              min={10}
              value={settings.fetchSize}
              onChange={(e) => updateSettings({ fetchSize: Math.max(10, Number(e.target.value) || 500) })}
            />
          </Field>
          <Field label="Editor font size">
            <Input
              type="number"
              min={9}
              max={28}
              value={settings.fontSize}
              onChange={(e) => updateSettings({ fontSize: Math.min(28, Math.max(9, Number(e.target.value) || 13)) })}
            />
          </Field>
        </div>
        <div className="grid grid-cols-2 items-end gap-3">
          <Field label="SQL formatter style">
            <Select value={settings.formatStyle} onChange={(e) => updateSettings({ formatStyle: e.target.value as "toad" | "standard" })}>
              <option value="toad">Toad (right-aligned keywords)</option>
              <option value="standard">Standard (indented)</option>
            </Select>
          </Field>
          <Checkbox
            label="Add “Formatted on …” header"
            checked={settings.formatHeader}
            onChange={(v) => updateSettings({ formatHeader: v })}
            className="pb-2"
          />
        </div>
        <div className="grid grid-cols-2 items-end gap-3">
          <Field label="Editor colours">
            <Select value={settings.editorScheme ?? "toad"} onChange={(e) => updateSettings({ editorScheme: e.target.value as "toad" | "vscode" })}>
              <option value="toad">Toad</option>
              <option value="vscode">VS Code</option>
            </Select>
          </Field>
          <Checkbox
            label="Colour existing table / view names"
            checked={settings.highlightObjects ?? true}
            onChange={(v) => updateSettings({ highlightObjects: v })}
            className="pb-2"
          />
        </div>
        <Checkbox
          label="Confirm UPDATE / DELETE without WHERE"
          checked={settings.confirmDml}
          onChange={(v) => updateSettings({ confirmDml: v })}
        />
        <div className="flex items-center justify-between rounded-md border border-line px-3 py-2">
          <span>{history.length} history entries</span>
          <Button size="sm" onClick={clearHistory}>
            Clear history
          </Button>
        </div>
        <div>
          <div className="mb-1 text-xs font-medium text-muted">Snippets ({snippets.length})</div>
          {snippets.map((sn) => (
            <div key={sn.name} className="flex items-center justify-between border-b border-line/60 py-1">
              <span className="truncate font-mono text-xs">{sn.name}</span>
              <Button size="sm" variant="ghost" onClick={() => deleteSnippet(sn.name)}>
                Delete
              </Button>
            </div>
          ))}
        </div>
        <div className="text-xs text-muted">Keyboard: Ctrl+Shift+N new connection · Ctrl+T new SQL tab</div>
      </div>
    </Modal>
  );
}
