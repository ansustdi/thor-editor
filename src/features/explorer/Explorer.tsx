import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Code2,
  Eye,
  FileCode,
  Hash,
  Package,
  RefreshCw,
  Search,
  Table2,
  Zap,
  Link2,
  Boxes,
  Layers,
  ListOrdered,
  Circle,
  ArrowRightLeft,
  KeyRound,
  FileJson,
  Network,
  Upload,
  BookOpen,
  AlertCircle,
  Database,
  Waves,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { IconButton, Input, Spinner } from "../../components/ui";
import { api, errorText, isSql, type DbObject, type ObjectSource } from "../../lib/ipc";
import { useApp, uid, type Connection } from "../../store/app";
import { MemberBadge, outline } from "./SourceView";

/** Oracle objects that expand into Spec / Body nodes in the tree, like Toad. */
const TREE_KINDS = new Set(["package", "type"]);

const KIND_ORDER = [
  "table", "view", "materialized view", "dictionary", "collection", "timeseries", "es index", "alias", "data stream", "label", "relationship",
  "string", "hash", "list", "set", "zset", "stream",
  "procedure", "function", "package", "sequence", "trigger", "synonym", "type", "index",
];
const KIND_LABEL: Record<string, string> = {
  table: "Tables",
  view: "Views",
  "materialized view": "Materialized Views",
  procedure: "Procedures",
  function: "Functions",
  package: "Packages",
  sequence: "Sequences",
  trigger: "Triggers",
  synonym: "Synonyms",
  type: "Types",
  index: "Indexes",
  dictionary: "Dictionaries",
  collection: "Collections",
  "es index": "Indices",
  alias: "Aliases",
  "data stream": "Data streams",
  timeseries: "Time series",
  label: "Node labels",
  relationship: "Relationship types",
  string: "Strings",
  hash: "Hashes",
  list: "Lists",
  set: "Sets",
  zset: "Sorted sets",
  stream: "Streams",
};

export const DATA_KINDS = new Set([
  "table", "view", "materialized view", "dictionary", "collection", "timeseries", "es index", "alias", "data stream", "label", "relationship",
  "string", "hash", "list", "set", "zset", "stream", "ReJSON-RL",
]);

export function kindIcon(kind: string, size = 14) {
  const p = { size, className: "shrink-0 text-muted" };
  switch (kind) {
    case "table":
      return <Table2 {...p} className="shrink-0 text-accent" />;
    case "view":
      return <Eye {...p} />;
    case "materialized view":
      return <Layers {...p} />;
    case "procedure":
      return <Code2 {...p} />;
    case "function":
      return <FileCode {...p} />;
    case "package":
      return <Package {...p} />;
    case "sequence":
      return <ListOrdered {...p} />;
    case "trigger":
      return <Zap {...p} />;
    case "synonym":
      return <Link2 {...p} />;
    case "index":
      return <Hash {...p} />;
    case "dictionary":
      return <BookOpen {...p} />;
    case "collection":
    case "timeseries":
      return <FileJson {...p} className="shrink-0 text-ok" />;
    case "es index":
      return <Database {...p} className="shrink-0 text-ok" />;
    case "alias":
      return <Link2 {...p} />;
    case "data stream":
      return <Waves {...p} className="shrink-0 text-ok" />;
    case "label":
      return <Circle {...p} className="shrink-0 text-accent" />;
    case "relationship":
      return <ArrowRightLeft {...p} />;
    case "string":
    case "hash":
    case "list":
    case "set":
    case "zset":
    case "stream":
      return <KeyRound {...p} className="shrink-0 text-danger" />;
    default:
      return <Boxes {...p} />;
  }
}

export function Explorer({ conn, onImport }: { conn: Connection; onImport: (schema: string, table?: string) => void }) {
  const { objects, cacheObjects, openTab } = useApp();
  const [schemas, setSchemas] = useState<string[]>([]);
  const [schema, setSchema] = useState(conn.info.currentSchema);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ trigger: true, synonym: true, index: true, type: true });

  const list = objects[conn.key]?.[schema];
  // Expandable package / type nodes: source loaded lazily on first expand.
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [sources, setSources] = useState<Record<string, ObjectSource | "loading" | { error: string }>>({});
  const treeable = (o: DbObject) => conn.info.dialect === "oracle" && TREE_KINDS.has(o.kind);
  const nodeKey = (o: DbObject, part = "") => `${o.schema}.${o.kind}.${o.name}${part && "|" + part}`;

  const toggle = async (o: DbObject) => {
    const k = nodeKey(o);
    const open = !expanded[k];
    setExpanded((e) => ({ ...e, [k]: open }));
    const cur = sources[k];
    if (!open || (cur && (cur === "loading" || !("error" in cur)))) return;
    setSources((m) => ({ ...m, [k]: "loading" }));
    try {
      const src = await api.objectSource(conn.info.sessionId, o.schema, o.name, o.kind);
      setSources((m) => ({ ...m, [k]: src }));
    } catch (e) {
      setSources((m) => ({ ...m, [k]: { error: errorText(e) } }));
    }
  };

  const openAt = (o: DbObject, part: string, line?: number) =>
    openTab({
      id: uid(),
      connKey: conn.key,
      kind: "object",
      title: o.name,
      object: o,
      view: "ddl",
      focus: { part, line, seq: Date.now() },
    });

  const renderChildren = (o: DbObject) => {
    const src = sources[nodeKey(o)];
    if (!src || src === "loading")
      return (
        <div className="flex items-center gap-1.5 py-[3px] pl-12 text-xs text-muted">
          <Spinner size={11} /> Loading…
        </div>
      );
    if ("error" in src) return <div className="py-[3px] pl-12 pr-2 text-xs text-danger select-text">{src.error}</div>;
    return src.parts.map((p) => {
      const pk = nodeKey(o, p.label);
      // Body is where the code lives; Spec starts collapsed unless it is the only part.
      const isOpen = expanded[pk] ?? (p.label !== "Spec" || src.parts.length === 1);
      const members = isOpen ? outline(p.text).filter((m) => m.kind === "procedure" || m.kind === "function") : [];
      return (
        <div key={p.label}>
          <div
            className="flex cursor-default items-center gap-1 py-[3px] pl-9 pr-2 hover:bg-hover"
            onClick={() => openAt(o, p.label)}
            title={`${o.name} ${p.label}${p.status ? ` — ${p.status}` : ""} — click to open`}
          >
            <button
              className="rounded hover:bg-panel-2"
              onClick={(e) => {
                e.stopPropagation();
                setExpanded((x) => ({ ...x, [pk]: !isOpen }));
              }}
            >
              {isOpen ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            </button>
            <FileCode size={13} className="shrink-0 text-muted" />
            <span className="truncate">{p.label}</span>
            {p.status === "INVALID" && <AlertCircle size={12} className="shrink-0 text-danger" />}
          </div>
          {isOpen && members.length === 0 && <div className="py-[2px] pl-[60px] text-xs text-muted">(no procedures or functions)</div>}
          {members.map((m, i) => (
            <div
              key={`${m.name}${m.line}${i}`}
              className="flex cursor-default items-center gap-1.5 py-[2px] pl-[60px] pr-2 hover:bg-hover"
              onClick={() => openAt(o, p.label, m.line)}
              title={`${m.kind} ${m.name}${m.returns ? " RETURN " + m.returns : ""} — line ${m.line}`}
            >
              <MemberBadge kind={m.kind} />
              <span className="truncate">
                {m.name}
                {m.returns && <span className="text-muted">: {m.returns}</span>}
              </span>
            </div>
          ))}
        </div>
      );
    });
  };

  const loadObjects = async (s: string, force = false) => {
    if (!s || (!force && objects[conn.key]?.[s])) return;
    if (force) setSources({});
    setLoading(true);
    setError("");
    try {
      cacheObjects(conn.key, s, await api.listObjects(conn.info.sessionId, s));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    api
      .listSchemas(conn.info.sessionId)
      .then((s) => {
        setSchemas(s);
        if (!schema || !s.includes(schema)) {
          const pick = s.find((x) => x.toLowerCase() === conn.info.currentSchema.toLowerCase()) ?? s[0] ?? "";
          setSchema(pick);
        }
      })
      .catch((e) => setError(errorText(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conn.info.sessionId]);

  useEffect(() => {
    loadObjects(schema);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [schema, list === undefined]);

  const grouped = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const m = new Map<string, DbObject[]>();
    for (const o of list ?? []) {
      if (q && !o.name.toLowerCase().includes(q)) continue;
      m.set(o.kind, [...(m.get(o.kind) ?? []), o]);
    }
    return [...m.entries()].sort(([a], [b]) => {
      const ia = KIND_ORDER.indexOf(a), ib = KIND_ORDER.indexOf(b);
      return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
    });
  }, [list, filter]);

  const open = (o: DbObject) =>
    openTab({ id: uid(), connKey: conn.key, kind: "object", title: o.name, object: o });

  return (
    <div className="flex h-full min-h-0 flex-col bg-panel">
      <div className="space-y-1.5 border-b border-line p-2">
        <div className="flex items-center gap-1">
          <select
            className="h-7 min-w-0 flex-1 rounded-md border border-line bg-panel px-1.5 text-xs font-medium outline-none"
            value={schema}
            onChange={(e) => setSchema(e.target.value)}
            title="Schema"
          >
            {schemas.map((s) => (
              <option key={s}>{s}</option>
            ))}
          </select>
          {isSql(conn.info.dialect) && conn.info.dialect !== "clickhouse" && (
            <IconButton
              title="ER diagram"
              onClick={() => openTab({ id: uid(), connKey: conn.key, kind: "er", title: `ER: ${schema}`, schema })}
            >
              <Network size={14} />
            </IconButton>
          )}
          {isSql(conn.info.dialect) && (
            <IconButton title="Import CSV / Excel into a table" onClick={() => onImport(schema)}>
              <Upload size={14} />
            </IconButton>
          )}
          <IconButton title="Refresh" onClick={() => loadObjects(schema, true)}>
            {loading ? <Spinner size={12} /> : <RefreshCw size={14} />}
          </IconButton>
        </div>
        <div className="relative">
          <Search size={13} className="absolute left-2 top-2 text-muted" />
          <Input className="h-7 pl-7 text-xs" placeholder="Filter objects…" value={filter} onChange={(e) => setFilter(e.target.value)} />
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto py-1 text-[13px]">
        {error && <div className="m-2 rounded bg-danger/10 p-2 text-xs text-danger select-text">{error}</div>}
        {!loading && list && list.length === 0 && <div className="p-3 text-xs text-muted">No objects in this schema.</div>}
        {grouped.map(([kind, items]) => {
          const isCollapsed = (collapsed[kind] ?? items.length > 300) && !filter;
          return (
            <div key={kind}>
              <button
                className="flex w-full items-center gap-1 px-2 py-1 text-xs font-semibold text-muted hover:bg-hover"
                onClick={() => setCollapsed((c) => ({ ...c, [kind]: !isCollapsed }))}
              >
                {isCollapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
                {KIND_LABEL[kind] ?? kind}
                <span className="font-normal">({items.length})</span>
              </button>
              {!isCollapsed &&
                items.map((o) => {
                  const tree = treeable(o);
                  const isOpen = tree && expanded[nodeKey(o)];
                  return (
                    <div key={o.kind + o.name}>
                      <div
                        className={clsx("flex cursor-default items-center gap-1.5 py-[3px] pr-2 hover:bg-hover", tree ? "pl-2" : "pl-6")}
                        onDoubleClick={() => open(o)}
                        onClick={(e) => e.detail === 1 && e.ctrlKey && open(o)}
                        draggable
                        onDragStart={(e) => e.dataTransfer.setData("text/plain", o.name)}
                        title={`${o.schema}.${o.name} — double-click to open`}
                      >
                        {tree && (
                          <button
                            className="-mr-0.5 rounded hover:bg-panel-2"
                            title={isOpen ? "Collapse" : "Show Spec / Body"}
                            onClick={(e) => {
                              e.stopPropagation();
                              toggle(o);
                            }}
                            onDoubleClick={(e) => e.stopPropagation()}
                          >
                            {isOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
                          </button>
                        )}
                        {kindIcon(o.kind)}
                        <span className="truncate">{o.name}</span>
                      </div>
                      {isOpen && renderChildren(o)}
                    </div>
                  );
                })}
            </div>
          );
        })}
      </div>
    </div>
  );
}
