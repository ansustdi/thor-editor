import { confirmAsk } from "../../lib/confirm";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import clsx from "clsx";
import {
  ArrowDownToLine,
  Copy,
  KeyRound,
  Download,
  FolderOpen,
  Plug,
  Plus,
  RefreshCw,
  Search,
  Star,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Checkbox, EngineBadge, Field, IconButton, Input, Modal, Select, Spinner, Tabs } from "../../components/ui";
import { describeTarget, engineInfo, ENGINES, newProfile, TAG_COLORS } from "../../lib/engines";
import { api, errorText, type ConnectionProfile, type Engine, type KnownSource, type OracleHome, type ProfileView } from "../../lib/ipc";
import { uid, useApp } from "../../store/app";

/** Toad identifies a saved connection by user + database. */
const identity = (p: ConnectionProfile) =>
  [p.engine, p.username.trim().toLowerCase(), describeTarget(p).trim().toLowerCase(), p.oracle?.connectAs ?? ""].join("|");

type GroupBy = "none" | "engine" | "folder" | "color";

export function LoginDialog() {
  const { addConnection, setLoginOpen, connections } = useApp();
  const [profiles, setProfiles] = useState<ProfileView[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [form, setForm] = useState<ConnectionProfile>(newProfile());
  const [password, setPassword] = useState("");
  const [sshPassword, setSshPassword] = useState("");
  const [importOpen, setImportOpen] = useState(false);
  const lastSaveNote = useRef("");
  const [hasSavedPw, setHasSavedPw] = useState(false);
  const [search, setSearch] = useState("");
  const [favOnly, setFavOnly] = useState(false);
  const [groupBy, setGroupBy] = useState<GroupBy>("none");
  const [busy, setBusy] = useState<"" | "test" | "connect" | "save">("");
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);
  const [homes, setHomes] = useState<OracleHome[]>([]);
  const [tnsAliases, setTnsAliases] = useState<string[]>([]);

  const reload = useCallback(async () => {
    try {
      setProfiles(await api.listProfiles());
    } catch (e) {
      setStatus({ ok: false, text: errorText(e) });
    }
  }, []);

  useEffect(() => {
    reload();
    api.oracleHomes().then(setHomes).catch(() => setHomes([]));
  }, [reload]);

  const selectedHome = homes.find((h) => h.libDir === form.oracle.clientLibDir) ?? (form.oracle.clientLibDir ? undefined : homes.find((h) => h.bundled));
  const configDir = form.oracle.configDir || selectedHome?.configDir || "";

  useEffect(() => {
    if (form.engine !== "oracle" || !configDir) {
      setTnsAliases([]);
      return;
    }
    api.tnsAliases(configDir).then(setTnsAliases).catch(() => setTnsAliases([]));
  }, [form.engine, configDir]);

  const pick = (p: ProfileView) => {
    setSelected(p.id);
    const { hasPassword, ...profile } = p;
    const base = newProfile(p.engine);
    setForm({ ...base, ...profile, oracle: { ...base.oracle, ...profile.oracle }, ssh: { ...base.ssh, ...profile.ssh }, params: profile.params ?? {} });
    setHasSavedPw(hasPassword);
    setPassword("");
    setSshPassword("");
    setStatus(null);
  };

  const startNew = (engine: Engine = form.engine) => {
    setSelected(null);
    setForm(newProfile(engine));
    setPassword("");
    setHasSavedPw(false);
    setStatus(null);
  };

  const set = <K extends keyof ConnectionProfile>(k: K, v: ConnectionProfile[K]) => setForm((f) => ({ ...f, [k]: v }));
  const setOra = <K extends keyof ConnectionProfile["oracle"]>(k: K, v: ConnectionProfile["oracle"][K]) =>
    setForm((f) => ({ ...f, oracle: { ...f.oracle, [k]: v } }));

  const changeEngine = (engine: Engine) => {
    setForm((f) => ({
      ...f,
      engine,
      port: engineInfo(engine).defaultPort,
      host: engine === "sqlite" ? "" : f.host || "localhost",
    }));
  };

  /**
   * Toad-style save: an entry is identified by engine + user + database. Editing other fields
   * updates it; changing the identity saves a new entry (or updates the matching one) and keeps
   * the original untouched.
   */
  const save = async (): Promise<ConnectionProfile | null> => {
    try {
      const original = profiles.find((p) => p.id === form.id);
      const twin = profiles.find((p) => p.id !== form.id && identity(p) === identity(form));
      let toSave = form;
      let copyFrom: string | undefined;
      let note = original ? "Connection updated." : "Connection saved.";
      if (original && identity(original) !== identity(form)) {
        toSave = {
          ...form,
          id: twin?.id ?? "",
          name: form.name === original.name ? (twin?.name ?? "") : form.name,
          favorite: twin?.favorite ?? false,
          lastUsed: null,
        };
        if (form.username.trim().toLowerCase() === original.username.trim().toLowerCase()) copyFrom = original.id;
        note = twin
          ? `Updated the existing ${form.username}@${describeTarget(form)} entry; ${original.username}@${describeTarget(original)} kept.`
          : `Saved as a new connection; ${original.username}@${describeTarget(original)} kept.`;
      } else if (!original && twin) {
        toSave = { ...form, id: twin.id };
        note = "Connection updated.";
      }
      const saved = await api.saveProfile(toSave, password || undefined, sshPassword || undefined, copyFrom);
      setForm(saved);
      setSelected(saved.id);
      if ((password || copyFrom) && saved.savePassword) setHasSavedPw(true);
      await reload();
      lastSaveNote.current = note;
      return saved;
    } catch (e) {
      setStatus({ ok: false, text: errorText(e) });
      return null;
    }
  };

  const test = async () => {
    setBusy("test");
    setStatus(null);
    try {
      const v = await api.testConnection(form, password || undefined, sshPassword || undefined);
      setStatus({ ok: true, text: `Connected: ${v.split("\n")[0]}` });
    } catch (e) {
      setStatus({ ok: false, text: errorText(e) });
    } finally {
      setBusy("");
    }
  };

  const connect = async (p?: ConnectionProfile) => {
    setBusy("connect");
    setStatus(null);
    try {
      let profile = p ?? form;
      if (!p) {
        const saved = await save();
        if (!saved) return;
        profile = saved;
      }
      const info = await api.connect(profile, p ? undefined : password || undefined, p ? undefined : sshPassword || undefined);
      const view = profiles.find((x) => x.id === profile.id) ?? { ...profile, hasPassword: false };
      addConnection({ key: uid("c"), profile: { ...view, ...profile }, info, autocommit: info.autocommit });
    } catch (e) {
      setStatus({ ok: false, text: errorText(e) });
    } finally {
      setBusy("");
    }
  };

  const remove = async () => {
    if (!selected) return;
    const p = profiles.find((x) => x.id === selected);
    if (!p || !(await confirmAsk(`Delete connection "${p.username}@${describeTarget(p)}"? The saved password is removed too.`, { danger: true }))) return;
    await api.deleteProfile(selected);
    startNew();
    reload();
  };

  const clone = () => {
    setSelected(null);
    setForm((f) => ({ ...f, id: "", name: f.name ? `${f.name} (copy)` : "" }));
    setHasSavedPw(false);
  };

  const toggleFav = async (p: ProfileView) => {
    const { hasPassword: _h, ...profile } = p;
    await api.saveProfile({ ...profile, favorite: !p.favorite });
    reload();
  };

  const doImport = async () => {
    const path = await openDialog({ filters: [{ name: "JSON", extensions: ["json"] }] });
    if (typeof path !== "string") return;
    try {
      const n = await api.importProfiles(path);
      setStatus({ ok: true, text: `Imported ${n} connection(s). Passwords are not included.` });
      reload();
    } catch (e) {
      setStatus({ ok: false, text: errorText(e) });
    }
  };

  const doExport = async () => {
    const path = await saveDialog({ defaultPath: "connections.json", filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!path) return;
    const n = await api.exportProfiles(path);
    setStatus({ ok: true, text: `Exported ${n} connection(s) (without passwords).` });
  };

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return profiles
      .filter((p) => !favOnly || p.favorite)
      .filter(
        (p) =>
          !q ||
          [p.username, p.name, describeTarget(p), p.folder, engineInfo(p.engine).label].some((s) =>
            s?.toLowerCase().includes(q),
          ),
      )
      .sort((a, b) => a.username.localeCompare(b.username) || describeTarget(a).localeCompare(describeTarget(b)));
  }, [profiles, search, favOnly]);

  const groups = useMemo(() => {
    if (groupBy === "none") return [{ key: "", items: filtered }];
    const m = new Map<string, ProfileView[]>();
    for (const p of filtered) {
      const k =
        groupBy === "engine"
          ? engineInfo(p.engine).label
          : groupBy === "folder"
            ? p.folder || "(no folder)"
            : TAG_COLORS.find((c) => c.value === p.color)?.name ?? "Custom";
      m.set(k, [...(m.get(k) ?? []), p]);
    }
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([key, items]) => ({ key, items }));
  }, [filtered, groupBy]);

  const eng = engineInfo(form.engine);
  const canClose = connections.length > 0;

  return (
    <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4">
      <div
        className="flex h-[680px] max-h-full w-[1080px] max-w-full flex-col overflow-hidden rounded-xl border border-line bg-panel shadow-2xl"
        onKeyDown={(e) => {
          const t = e.target as HTMLElement;
          if (e.key === "Enter" && !(t instanceof HTMLButtonElement) && !t.dataset.noenter) connect();
          if (e.key === "Escape" && canClose) setLoginOpen(false);
        }}
      >
        {/* Title */}
        <div className="flex items-center gap-2 border-b border-line px-4 py-2.5">
          <Plug size={16} className="text-accent" />
          <div className="text-sm font-semibold">Database Login</div>
          <div className="flex-1" />
          {canClose && (
            <IconButton title="Close" onClick={() => setLoginOpen(false)}>
              <X size={16} />
            </IconButton>
          )}
        </div>

        <div className="flex min-h-0 flex-1">
          {/* Left: saved connections */}
          <div className="flex min-w-0 flex-1 flex-col border-r border-line">
            <div className="flex items-center gap-1 border-b border-line px-2 py-1.5">
              <IconButton title="New connection" onClick={() => startNew()}>
                <Plus size={16} />
              </IconButton>
              <IconButton title="Clone connection" onClick={clone} disabled={!selected}>
                <Copy size={15} />
              </IconButton>
              <IconButton title="Delete connection" onClick={remove} disabled={!selected}>
                <Trash2 size={15} />
              </IconButton>
              <div className="mx-1 h-5 w-px bg-line" />
              <IconButton title="Import connections (JSON)" onClick={doImport}>
                <Upload size={15} />
              </IconButton>
              <IconButton title="Import from Toad / DBeaver / DataGrip" onClick={() => setImportOpen(true)}>
                <ArrowDownToLine size={15} />
              </IconButton>
              <IconButton title="Export connections (JSON)" onClick={doExport}>
                <Download size={15} />
              </IconButton>
              <IconButton title="Refresh" onClick={reload}>
                <RefreshCw size={15} />
              </IconButton>
              <div className="relative ml-2 flex-1">
                <Search size={14} className="absolute left-2 top-2 text-muted" />
                <Input data-noenter="1" placeholder="Search connections…" className="pl-7" value={search} onChange={(e) => setSearch(e.target.value)} />
              </div>
              <Select className="w-36" value={groupBy} onChange={(e) => setGroupBy(e.target.value as GroupBy)} title="Group by">
                <option value="none">No grouping</option>
                <option value="engine">Group: Engine</option>
                <option value="folder">Group: Folder</option>
                <option value="color">Group: Color</option>
              </Select>
            </div>

            <div className="grid grid-cols-[28px_36px_minmax(120px,1fr)_minmax(160px,1.6fr)] border-b border-line bg-panel-2 px-2 py-1.5 text-xs font-semibold text-muted">
              <span />
              <span />
              <span>User</span>
              <span>Database</span>
            </div>
            <div className="min-h-0 flex-1 overflow-auto">
              {filtered.length === 0 && (
                <div className="p-8 text-center text-muted">
                  {profiles.length === 0 ? "No saved connections yet. Fill in the form to create one." : "No matches."}
                </div>
              )}
              {groups.map((g) => (
                <div key={g.key}>
                  {g.key && (
                    <div className="sticky top-0 z-10 bg-panel-2/95 px-3 py-1 text-xs font-semibold text-muted">
                      {g.key} <span className="font-normal">({g.items.length})</span>
                    </div>
                  )}
                  {g.items.map((p) => {
                    const info = engineInfo(p.engine);
                    return (
                      <div
                        key={p.id}
                        onClick={() => pick(p)}
                        onDoubleClick={() => connect(p)}
                        className={clsx(
                          "grid cursor-default grid-cols-[28px_36px_minmax(120px,1fr)_minmax(160px,1.6fr)] items-center border-b border-line/60 px-2 py-1",
                          selected === p.id ? "bg-accent-soft" : "hover:bg-hover",
                        )}
                        style={p.color ? { boxShadow: `inset 3px 0 0 ${p.color}` } : undefined}
                      >
                        <button
                          title={p.favorite ? "Remove from favorites" : "Add to favorites"}
                          onClick={(e) => {
                            e.stopPropagation();
                            toggleFav(p);
                          }}
                          className="text-muted hover:text-warn"
                        >
                          <Star size={14} fill={p.favorite ? "var(--warn)" : "none"} color={p.favorite ? "var(--warn)" : "currentColor"} />
                        </button>
                        <EngineBadge short={info.short} color={info.color} size={24} />
                        <span className="truncate font-medium">
                          {p.username || <i className="text-muted">(os auth)</i>}
                          {p.name && <span className="ml-1.5 text-xs font-normal text-muted">{p.name}</span>}
                        </span>
                        <span className="truncate font-mono text-xs">{describeTarget(p)}</span>
                      </div>
                    );
                  })}
                </div>
              ))}
            </div>
            <div className="flex items-center gap-5 border-t border-line px-3 py-2">
              <Checkbox label="Save password" checked={form.savePassword} onChange={(v) => set("savePassword", v)} />
              <Checkbox label="Show favorites only" checked={favOnly} onChange={setFavOnly} />
              <span className="ml-auto text-xs text-muted">{profiles.length} saved · double-click to connect</span>
            </div>
          </div>

          {/* Right: form */}
          <div className="flex w-[400px] shrink-0 flex-col">
            <div className="min-h-0 flex-1 space-y-3 overflow-auto p-4">
              <Field label="Database type">
                <div className="grid grid-cols-3 gap-1.5">
                  {ENGINES.map((e) => (
                    <button
                      key={e.id}
                      onClick={() => changeEngine(e.id)}
                      className={clsx(
                        "flex items-center gap-1.5 rounded-md border px-2 py-1.5 text-xs",
                        form.engine === e.id ? "border-accent bg-accent-soft font-semibold" : "border-line hover:bg-hover",
                      )}
                    >
                      <EngineBadge short={e.short} color={e.color} size={18} />
                      {e.label}
                    </button>
                  ))}
                </div>
              </Field>

              {form.engine !== "sqlite" && (
                <div className="grid grid-cols-2 gap-2">
                  <Field label="User">
                    <Input
                      autoFocus
                      value={form.username}
                      onChange={(e) => set("username", e.target.value)}
                      disabled={form.engine === "mssql" && form.auth === "windows"}
                    />
                  </Field>
                  <Field label="Password">
                    <Input
                      type="password"
                      value={password}
                      placeholder={hasSavedPw ? "•••••••• (saved)" : form.engine === "elasticsearch" && !form.username ? "or API key (empty user)" : ""}
                      onChange={(e) => setPassword(e.target.value)}
                      disabled={form.engine === "mssql" && form.auth === "windows"}
                    />
                  </Field>
                </div>
              )}

              {form.engine === "oracle" && (
                <OracleForm form={form} setOra={setOra} set={set} homes={homes} tnsAliases={tnsAliases} configDir={configDir} />
              )}

              {form.engine === "sqlite" && (
                <Field label="Database file">
                  <div className="flex gap-1.5">
                    <Input value={form.database} onChange={(e) => set("database", e.target.value)} placeholder="C:\data\app.db" />
                    <Button
                      onClick={async () => {
                        const p = await openDialog({
                          filters: [{ name: "SQLite", extensions: ["db", "sqlite", "sqlite3", "db3"] }, { name: "All", extensions: ["*"] }],
                        });
                        if (typeof p === "string") set("database", p);
                      }}
                    >
                      <FolderOpen size={14} />
                    </Button>
                  </div>
                </Field>
              )}

              {form.engine === "mongodb" && (
                <Field label="Connection string (optional — overrides the fields below)">
                  <Input
                    value={form.params.uri ?? ""}
                    placeholder="mongodb+srv://user:pass@cluster0.example.net/?retryWrites=true"
                    onChange={(e) => set("params", { ...form.params, uri: e.target.value })}
                  />
                </Field>
              )}

              {!["oracle", "sqlite"].includes(form.engine) && (
                <>
                  <div className="grid grid-cols-[1fr_90px] gap-2">
                    <Field label="Host">
                      <Input
                        value={form.host}
                        placeholder={form.engine === "elasticsearch" ? "localhost or https://… (cloud URL)" : ""}
                        onChange={(e) => set("host", e.target.value)}
                      />
                    </Field>
                    <Field label="Port">
                      <Input
                        type="number"
                        value={form.port || ""}
                        placeholder={String(eng.defaultPort)}
                        onChange={(e) => set("port", Number(e.target.value) || 0)}
                      />
                    </Field>
                  </div>
                  <div className="grid grid-cols-2 gap-2">
                    {form.engine !== "elasticsearch" && (
                      <Field label={DB_LABEL[form.engine] ?? "Database"}>
                        <Input
                          value={form.database}
                          placeholder={DB_PLACEHOLDER[form.engine] ?? ""}
                          onChange={(e) => set("database", e.target.value)}
                        />
                      </Field>
                    )}
                    <Field label="SSL">
                      <Select value={form.sslMode} onChange={(e) => set("sslMode", e.target.value)}>
                        <option value="">Default</option>
                        <option value="disable">Disable</option>
                        <option value="prefer">Prefer</option>
                        <option value="require">Require (no verify)</option>
                        <option value="verify-full">Verify full</option>
                      </Select>
                    </Field>
                  </div>
                  {form.engine === "mongodb" && !form.params.uri && (
                    <Field label="Auth database">
                      <Input
                        value={form.params.authSource ?? ""}
                        placeholder="admin"
                        onChange={(e) => set("params", { ...form.params, authSource: e.target.value })}
                      />
                    </Field>
                  )}
                  {form.engine === "mssql" && (
                    <Field label="Authentication">
                      <Select value={form.auth || "sql"} onChange={(e) => set("auth", e.target.value)}>
                        <option value="sql">SQL Server authentication</option>
                        <option value="windows">Windows authentication</option>
                      </Select>
                    </Field>
                  )}
                </>
              )}

              {form.engine !== "sqlite" && (
                <SshSection
                  ssh={form.ssh}
                  onChange={(ssh) => set("ssh", ssh)}
                  password={sshPassword}
                  onPassword={setSshPassword}
                  hasSaved={hasSavedPw}
                />
              )}

              <div className="grid grid-cols-2 gap-2">
                <Field label="Color">
                  <div className="flex items-center gap-1.5">
                    <span className="h-5 w-5 shrink-0 rounded border border-line" style={{ background: form.color || "transparent" }} />
                    <Select value={form.color} onChange={(e) => set("color", e.target.value)}>
                      {TAG_COLORS.map((c) => (
                        <option key={c.value} value={c.value}>
                          {c.name}
                        </option>
                      ))}
                    </Select>
                  </div>
                </Field>
                <Field label="Auto-commit">
                  <Select
                    value={form.autocommit === null ? "" : form.autocommit ? "on" : "off"}
                    onChange={(e) => set("autocommit", e.target.value === "" ? null : e.target.value === "on")}
                  >
                    <option value="">Default ({form.engine === "oracle" ? "off" : "on"})</option>
                    <option value="on">On</option>
                    <option value="off">Off (manual commit)</option>
                  </Select>
                </Field>
                <Field label="Name (optional)">
                  <Input value={form.name} onChange={(e) => set("name", e.target.value)} />
                </Field>
                <Field label="Folder">
                  <Input value={form.folder} onChange={(e) => set("folder", e.target.value)} placeholder="e.g. Production" />
                </Field>
              </div>
            </div>

            {status && (
              <div
                className={clsx(
                  "mx-4 mb-2 max-h-24 overflow-auto rounded-md px-3 py-2 text-xs select-text",
                  status.ok ? "bg-ok/10 text-ok" : "bg-danger/10 text-danger",
                )}
              >
                {status.text}
              </div>
            )}
            <div className="flex items-center gap-2 border-t border-line px-4 py-2.5">
              <Button onClick={test} disabled={!!busy}>
                {busy === "test" && <Spinner size={12} />} Test
              </Button>
              <Button onClick={async () => { setBusy("save"); const s = await save(); setBusy(""); if (s) setStatus({ ok: true, text: lastSaveNote.current }); }} disabled={!!busy}>
                Save
              </Button>
              <div className="flex-1" />
              <Button variant="primary" onClick={() => connect()} disabled={!!busy} className="min-w-24">
                {busy === "connect" ? <Spinner size={12} /> : <Plug size={14} />} Connect
              </Button>
              {canClose && <Button onClick={() => setLoginOpen(false)}>Close</Button>}
            </div>
          </div>
        </div>
      </div>
      {importOpen && (
        <ImportConnectionsDialog
          onClose={() => setImportOpen(false)}
          onDone={(n) => {
            setImportOpen(false);
            setStatus({ ok: true, text: `Imported ${n} connection(s). Enter passwords on first connect.` });
            reload();
          }}
        />
      )}
    </div>
  );
}

const DB_LABEL: Partial<Record<Engine, string>> = { redis: "DB index", neo4j: "Database", clickhouse: "Database", janusgraph: "Traversal source" };
const DB_PLACEHOLDER: Partial<Record<Engine, string>> = { redis: "0", neo4j: "neo4j", clickhouse: "default", mongodb: "test", janusgraph: "g" };

function SshSection({
  ssh,
  onChange,
  password,
  onPassword,
  hasSaved,
}: {
  ssh: ConnectionProfile["ssh"];
  onChange: (s: ConnectionProfile["ssh"]) => void;
  password: string;
  onPassword: (v: string) => void;
  hasSaved: boolean;
}) {
  const set = <K extends keyof ConnectionProfile["ssh"]>(k: K, v: ConnectionProfile["ssh"][K]) => onChange({ ...ssh, [k]: v });
  return (
    <div className="rounded-md border border-line">
      <label className="flex cursor-pointer items-center gap-2 px-3 py-2 text-[13px]">
        <input type="checkbox" className="h-4 w-4 accent-[var(--accent)]" checked={ssh.enabled} onChange={(e) => set("enabled", e.target.checked)} />
        <KeyRound size={14} className="text-muted" /> SSH tunnel
        {ssh.enabled && ssh.host && (
          <span className="ml-auto truncate text-xs text-muted">
            {ssh.user}@{ssh.host}
          </span>
        )}
      </label>
      {ssh.enabled && (
        <div className="space-y-2 border-t border-line p-3">
          <div className="grid grid-cols-[1fr_80px] gap-2">
            <Field label="SSH host">
              <Input value={ssh.host} onChange={(e) => set("host", e.target.value)} placeholder="bastion.example.com" />
            </Field>
            <Field label="Port">
              <Input type="number" value={ssh.port || ""} placeholder="22" onChange={(e) => set("port", Number(e.target.value) || 0)} />
            </Field>
          </div>
          <div className="grid grid-cols-2 gap-2">
            <Field label="SSH user">
              <Input value={ssh.user} onChange={(e) => set("user", e.target.value)} />
            </Field>
            <Field label={ssh.keyPath ? "Key passphrase" : "SSH password"}>
              <Input type="password" value={password} placeholder={hasSaved ? "(saved)" : ""} onChange={(e) => onPassword(e.target.value)} />
            </Field>
          </div>
          <Field label="Private key (optional, OpenSSH/PEM)">
            <div className="flex gap-1.5">
              <Input value={ssh.keyPath} onChange={(e) => set("keyPath", e.target.value)} placeholder="~/.ssh/id_ed25519" />
              <Button
                onClick={async () => {
                  const f = await openDialog({});
                  if (typeof f === "string") set("keyPath", f);
                }}
              >
                <FolderOpen size={14} />
              </Button>
            </div>
          </Field>
          <div className="text-[11px] text-muted">
            Host and port above are resolved from the SSH server. New host keys are saved to ~/.ssh/known_hosts; a changed key is rejected.
          </div>
        </div>
      )}
    </div>
  );
}

function ImportConnectionsDialog({ onClose, onDone }: { onClose: () => void; onDone: (n: number) => void }) {
  const [sources, setSources] = useState<KnownSource[]>([]);
  const [found, setFound] = useState<ConnectionProfile[]>([]);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.connectionSources().then(setSources).catch(() => setSources([]));
  }, []);

  const load = async (kind: string, path: string) => {
    setError("");
    try {
      const list = await api.parseConnections(kind, path);
      setFound(list);
      setPicked(new Set(list.map((_, i) => i)));
      if (!list.length) setError("No connections found in that file.");
    } catch (e) {
      setError(errorText(e));
    }
  };

  const browse = async (kind: string) => {
    const ext = kind === "dbeaver" ? "json" : "xml";
    const f = await openDialog({ filters: [{ name: ext.toUpperCase(), extensions: [ext] }] });
    if (typeof f === "string") load(kind, f);
  };

  const doImport = async () => {
    setBusy(true);
    let n = 0;
    try {
      for (const i of picked) {
        await api.saveProfile({ ...found[i], savePassword: true });
        n++;
      }
      onDone(n);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Import connections"
      onClose={onClose}
      width={760}
      footer={
        <>
          <span className="mr-auto text-xs text-muted">Passwords are not imported (the source tool encrypts them).</span>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={!picked.size || busy} onClick={doImport}>
            Import {picked.size || ""}
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="flex flex-wrap gap-2">
          {sources.length === 0 && <span className="text-xs text-muted">No Toad, DBeaver or DataGrip files found in the default locations.</span>}
          {sources.map((s) => (
            <Button key={s.path} onClick={() => load(s.kind, s.path)} title={s.path}>
              {s.label}
            </Button>
          ))}
          <div className="ml-auto flex gap-1">
            <Button size="sm" variant="ghost" onClick={() => browse("toad")}>
              Toad file…
            </Button>
            <Button size="sm" variant="ghost" onClick={() => browse("dbeaver")}>
              DBeaver file…
            </Button>
            <Button size="sm" variant="ghost" onClick={() => browse("datagrip")}>
              DataGrip file…
            </Button>
          </div>
        </div>
        {error && <div className="rounded bg-danger/10 px-3 py-2 text-xs text-danger">{error}</div>}
        {found.length > 0 && (
          <div className="max-h-96 overflow-auto rounded-md border border-line">
            <div className="sticky top-0 flex items-center gap-2 border-b border-line bg-panel-2 px-3 py-1.5 text-xs">
              <Checkbox
                label={`${picked.size} of ${found.length} selected`}
                checked={picked.size === found.length}
                onChange={(v) => setPicked(v ? new Set(found.map((_, i) => i)) : new Set())}
              />
            </div>
            {found.map((p, i) => {
              const info = engineInfo(p.engine);
              return (
                <label key={p.id} className="flex cursor-pointer items-center gap-2 border-b border-line/60 px-3 py-1 hover:bg-hover">
                  <input
                    type="checkbox"
                    className="accent-[var(--accent)]"
                    checked={picked.has(i)}
                    onChange={(e) => {
                      const next = new Set(picked);
                      if (e.target.checked) next.add(i);
                      else next.delete(i);
                      setPicked(next);
                    }}
                  />
                  <EngineBadge short={info.short} color={info.color} size={18} />
                  <span className="w-40 truncate font-medium">{p.username || p.name}</span>
                  <span className="truncate font-mono text-xs">{describeTarget(p)}</span>
                  {p.folder && <span className="ml-auto text-xs text-muted">{p.folder}</span>}
                </label>
              );
            })}
          </div>
        )}
      </div>
    </Modal>
  );
}

function OracleForm({
  form,
  set,
  setOra,
  homes,
  tnsAliases,
  configDir,
}: {
  form: ConnectionProfile;
  set: <K extends keyof ConnectionProfile>(k: K, v: ConnectionProfile[K]) => void;
  setOra: <K extends keyof ConnectionProfile["oracle"]>(k: K, v: ConnectionProfile["oracle"][K]) => void;
  homes: OracleHome[];
  tnsAliases: string[];
  configDir: string;
}) {
  const o = form.oracle;
  return (
    <>
      <Tabs
        value={o.method}
        onChange={(v) => setOra("method", v)}
        items={[
          { value: "tns", label: "TNS" },
          { value: "direct", label: "Direct" },
          { value: "ldap", label: "LDAP" },
        ]}
      />
      {o.method === "direct" && (
        <div className="space-y-2 rounded-md border border-line p-3">
          <div className="grid grid-cols-[1fr_90px] gap-2">
            <Field label="Host">
              <Input value={form.host} onChange={(e) => set("host", e.target.value)} />
            </Field>
            <Field label="Port">
              <Input type="number" value={form.port || ""} placeholder="1521" onChange={(e) => set("port", Number(e.target.value) || 0)} />
            </Field>
          </div>
          <div className="flex gap-4 text-[13px]">
            {(["service", "sid"] as const).map((t) => (
              <label key={t} className="inline-flex items-center gap-1.5">
                <input type="radio" className="accent-[var(--accent)]" checked={o.serviceType === t} onChange={() => setOra("serviceType", t)} />
                {t === "service" ? "Service Name" : "SID"}
              </label>
            ))}
          </div>
          <Input value={o.service} onChange={(e) => setOra("service", e.target.value)} placeholder={o.serviceType === "sid" ? "ORCL" : "ORCLPDB1"} />
        </div>
      )}
      {o.method === "tns" && (
        <div className="space-y-2 rounded-md border border-line p-3">
          <Field label={`TNS alias${configDir ? "" : " (no tnsnames.ora found)"}`}>
            <Input list="tns-aliases" value={o.tnsAlias} onChange={(e) => setOra("tnsAlias", e.target.value)} placeholder="PROD" />
            <datalist id="tns-aliases">
              {tnsAliases.map((a) => (
                <option key={a} value={a} />
              ))}
            </datalist>
          </Field>
          <Field label="TNS_ADMIN (config dir)">
            <Input value={o.configDir} onChange={(e) => setOra("configDir", e.target.value)} placeholder={configDir || "auto: <client>\\network\\admin"} />
          </Field>
          {tnsAliases.length > 0 && <div className="text-xs text-muted">{tnsAliases.length} aliases in tnsnames.ora</div>}
        </div>
      )}
      {o.method === "ldap" && (
        <div className="space-y-2 rounded-md border border-line p-3">
          <Field label="Service name (resolved via ldap.ora)">
            <Input value={o.ldapAlias} onChange={(e) => setOra("ldapAlias", e.target.value)} />
          </Field>
          <Field label="Config dir containing ldap.ora / sqlnet.ora">
            <Input value={o.configDir} onChange={(e) => setOra("configDir", e.target.value)} placeholder={configDir || "auto"} />
          </Field>
        </div>
      )}
      <div className="grid grid-cols-2 gap-2">
        <Field label="Connect as">
          <Select value={o.connectAs} onChange={(e) => setOra("connectAs", e.target.value)}>
            {["normal", "sysdba", "sysoper", "sysasm", "sysbackup", "sysdg", "syskm"].map((v) => (
              <option key={v} value={v}>
                {v === "normal" ? "Normal" : v.toUpperCase()}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Oracle client">
          <Select value={o.clientLibDir} onChange={(e) => setOra("clientLibDir", e.target.value)}>
            <option value="">{homes.some((h) => h.bundled) ? "Bundled Instant Client" : "Auto (PATH)"}</option>
            {homes
              .filter((h) => !h.bundled)
              .map((h) => (
                <option key={h.libDir} value={h.libDir}>
                  {h.name}
                </option>
              ))}
          </Select>
        </Field>
      </div>
      {homes.length === 0 && (
        <div className="rounded-md bg-warn/10 px-3 py-2 text-xs text-warn">
          No Oracle client detected. Install Oracle Instant Client (Basic Light) and add it to PATH, or place it in the app's
          <code className="mx-1">instantclient</code> resource folder.
        </div>
      )}
    </>
  );
}
