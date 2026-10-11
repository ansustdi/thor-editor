import { create } from "zustand";
import { persist } from "zustand/middleware";
import type { ColumnInfo, ConnectInfo, DbObject, ProfileView } from "../lib/ipc";

export interface Connection {
  key: string;
  profile: ProfileView;
  info: ConnectInfo;
  autocommit: boolean;
  /** Result of the last check / keep-alive. */
  status?: "ok" | "lost" | "checking";
  /** DBMS_OUTPUT capture (Oracle); undefined = not set up yet. */
  serverOutput?: boolean;
  statusText?: string;
}

/** Explorer-tree navigation into a Spec/Body part (and line); `seq` makes repeat clicks re-apply. */
export interface SourceFocus {
  part: string;
  line?: number;
  seq: number;
}

export type EditorTab =
  | { id: string; connKey: string; kind: "sql"; title: string; sql: string }
  | { id: string; connKey: string; kind: "object"; title: string; object: DbObject; view?: "data" | "columns" | "ddl"; focus?: SourceFocus }
  | { id: string; connKey: string; kind: "er"; title: string; schema: string }
  | { id: string; connKey: string; kind: "sessions"; title: string };

export interface HistoryEntry {
  sql: string;
  conn: string;
  at: string;
  ms: number;
  ok: boolean;
}

export interface Snippet {
  name: string;
  sql: string;
}

export interface Settings {
  theme: "light" | "dark" | "system";
  fetchSize: number;
  fontSize: number;
  confirmDml: boolean;
  formatStyle: "toad" | "standard";
  formatHeader: boolean;
  /** F5 script: stop at the first error or keep going (Toad's script mode). */
  scriptOnError: "stop" | "continue";
  /** Editor colours: Toad (default) or VS Code. */
  editorScheme: "toad" | "vscode";
  /** Colour table / view / package names that exist in the database (Toad-style). */
  highlightObjects: boolean;
}

interface State {
  connections: Connection[];
  activeConn: string | null;
  tabs: EditorTab[];
  activeTab: Record<string, string>;
  loginOpen: boolean;
  settingsOpen: boolean;
  objects: Record<string, Record<string, DbObject[]>>;
  columns: Record<string, Record<string, ColumnInfo[]>>;
  history: HistoryEntry[];
  snippets: Snippet[];
  settings: Settings;

  addConnection(c: Connection): void;
  closeConnection(key: string): void;
  setActiveConn(key: string): void;
  setAutocommit(key: string, on: boolean): void;
  setConnStatus(key: string, status: Connection["status"], text?: string): void;
  setConnServerOutput(key: string, on: boolean): void;
  openTab(tab: EditorTab): void;
  closeTab(id: string): void;
  setActiveTab(connKey: string, id: string): void;
  updateSql(id: string, sql: string): void;
  setLoginOpen(v: boolean): void;
  setSettingsOpen(v: boolean): void;
  cacheObjects(connKey: string, schema: string, list: DbObject[]): void;
  invalidateObjects(connKey: string): void;
  cacheColumns(connKey: string, table: string, list: ColumnInfo[]): void;
  addHistory(h: HistoryEntry): void;
  clearHistory(): void;
  saveSnippet(s: Snippet): void;
  deleteSnippet(name: string): void;
  updateSettings(s: Partial<Settings>): void;
}

let seq = 0;
export const uid = (p = "t") => `${p}${Date.now().toString(36)}${(seq++).toString(36)}`;

export const useApp = create<State>()(
  persist(
    (set) => ({
      connections: [],
      activeConn: null,
      tabs: [],
      activeTab: {},
      loginOpen: true,
      settingsOpen: false,
      objects: {},
      columns: {},
      history: [],
      snippets: [],
      settings: { theme: "system", fetchSize: 500, fontSize: 13, confirmDml: true, formatStyle: "toad", formatHeader: true, scriptOnError: "stop", editorScheme: "toad", highlightObjects: true },

      addConnection: (c) =>
        set((s) => {
          const tab: EditorTab = { id: uid(), connKey: c.key, kind: "sql", title: "SQL 1", sql: "" };
          return {
            connections: [...s.connections, c],
            activeConn: c.key,
            tabs: [...s.tabs, tab],
            activeTab: { ...s.activeTab, [c.key]: tab.id },
            loginOpen: false,
          };
        }),
      closeConnection: (key) =>
        set((s) => {
          const connections = s.connections.filter((c) => c.key !== key);
          const { [key]: _o, ...objects } = s.objects;
          const { [key]: _c, ...columns } = s.columns;
          return {
            connections,
            tabs: s.tabs.filter((t) => t.connKey !== key),
            activeConn: s.activeConn === key ? (connections[connections.length - 1]?.key ?? null) : s.activeConn,
            objects,
            columns,
            loginOpen: connections.length === 0 ? true : s.loginOpen,
          };
        }),
      setActiveConn: (key) => set({ activeConn: key }),
      setConnServerOutput: (key, on) =>
        set((s) => ({ connections: s.connections.map((c) => (c.key === key ? { ...c, serverOutput: on } : c)) })),
      setConnStatus: (key, status, text) =>
        set((s) => ({ connections: s.connections.map((c) => (c.key === key ? { ...c, status, statusText: text } : c)) })),
      setAutocommit: (key, on) =>
        set((s) => ({ connections: s.connections.map((c) => (c.key === key ? { ...c, autocommit: on } : c)) })),
      openTab: (tab) =>
        set((s) => {
          const existing =
            tab.kind === "object"
              ? s.tabs.find(
                  (t) =>
                    t.kind === "object" &&
                    t.connKey === tab.connKey &&
                    t.object.schema === tab.object.schema &&
                    t.object.name === tab.object.name,
                )
              : undefined;
          if (existing) {
            const activeTab = { ...s.activeTab, [tab.connKey]: existing.id };
            if (tab.kind !== "object" || !tab.focus) return { activeTab };
            const focus = tab.focus;
            return { activeTab, tabs: s.tabs.map((t) => (t.id === existing.id ? { ...t, focus } : t)) };
          }
          return { tabs: [...s.tabs, tab], activeTab: { ...s.activeTab, [tab.connKey]: tab.id } };
        }),
      closeTab: (id) =>
        set((s) => {
          const tab = s.tabs.find((t) => t.id === id);
          if (!tab) return {};
          const tabs = s.tabs.filter((t) => t.id !== id);
          const siblings = tabs.filter((t) => t.connKey === tab.connKey);
          const activeTab = { ...s.activeTab };
          if (activeTab[tab.connKey] === id) activeTab[tab.connKey] = siblings[siblings.length - 1]?.id ?? "";
          return { tabs, activeTab };
        }),
      setActiveTab: (connKey, id) => set((s) => ({ activeTab: { ...s.activeTab, [connKey]: id } })),
      updateSql: (id, sql) =>
        set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.kind === "sql" ? { ...t, sql } : t)) })),
      setLoginOpen: (v) => set({ loginOpen: v }),
      setSettingsOpen: (v) => set({ settingsOpen: v }),
      cacheObjects: (k, schema, list) =>
        set((s) => ({ objects: { ...s.objects, [k]: { ...(s.objects[k] ?? {}), [schema]: list } } })),
      invalidateObjects: (k) =>
        set((s) => ({ objects: { ...s.objects, [k]: {} }, columns: { ...s.columns, [k]: {} } })),
      cacheColumns: (k, table, list) =>
        set((s) => ({ columns: { ...s.columns, [k]: { ...(s.columns[k] ?? {}), [table]: list } } })),
      addHistory: (h) => set((s) => ({ history: [h, ...s.history].slice(0, 500) })),
      clearHistory: () => set({ history: [] }),
      saveSnippet: (sn) => set((s) => ({ snippets: [...s.snippets.filter((x) => x.name !== sn.name), sn] })),
      deleteSnippet: (name) => set((s) => ({ snippets: s.snippets.filter((x) => x.name !== name) })),
      updateSettings: (p) => set((s) => ({ settings: { ...s.settings, ...p } })),
    }),
    {
      // Pre-rename (DbForge Lite) key, kept so history / snippets / settings survive.
      name: "dbforge-lite",
      partialize: (s) => ({ history: s.history, snippets: s.snippets, settings: s.settings }),
      // Keep defaults for settings keys added after the user's data was saved.
      merge: (persisted, current) => {
        const p = (persisted ?? {}) as Partial<State>;
        return { ...current, ...p, settings: { ...current.settings, ...(p.settings ?? {}) } };
      },
    },
  ),
);
