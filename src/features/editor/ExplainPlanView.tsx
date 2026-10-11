import clsx from "clsx";
import { ChevronDown, ChevronRight, Copy, FileText, Network } from "lucide-react";
import { useMemo, useState } from "react";
import { Button } from "../../components/ui";
import type { Plan, PlanNode } from "../../lib/ipc";

interface TreeNode extends PlanNode {
  children: TreeNode[];
  depth: number;
}

function build(nodes: PlanNode[]): TreeNode[] {
  const map = new Map<number, TreeNode>();
  nodes.forEach((n) => map.set(n.id, { ...n, children: [], depth: 0 }));
  const roots: TreeNode[] = [];
  for (const n of map.values()) {
    const p = n.parentId !== null && n.parentId !== undefined ? map.get(n.parentId) : undefined;
    if (p) p.children.push(n);
    else roots.push(n);
  }
  const setDepth = (n: TreeNode, d: number) => {
    n.depth = d;
    n.children.forEach((c) => setDepth(c, d + 1));
  };
  roots.forEach((r) => setDepth(r, 0));
  return roots;
}

const fmt = (v: number | null | undefined) => (v === null || v === undefined ? null : Math.round(v).toLocaleString("en-US"));

/** Toad-like colour cue: full scans red, index access blue, joins purple. */
function tone(n: PlanNode): { dot: string; text: string } {
  const op = `${n.operation} ${n.options}`.toUpperCase();
  if (/FULL|SEQ SCAN/.test(op)) return { dot: "bg-danger", text: "text-danger" };
  if (/INDEX/.test(op)) return { dot: "bg-accent", text: "text-accent" };
  if (/JOIN|NESTED LOOP|HASH|MERGE/.test(op)) return { dot: "bg-purple-500", text: "text-purple-600 dark:text-purple-400" };
  if (/SORT|AGGREGATE|GROUP/.test(op)) return { dot: "bg-warn", text: "text-warn" };
  return { dot: "bg-muted", text: "text-fg" };
}

function Row({ n, collapsed, toggle, index }: { n: TreeNode; collapsed: Set<number>; toggle: (id: number) => void; index: number }) {
  const t = tone(n);
  const isRoot = n.depth === 0;
  const open = !collapsed.has(n.id);
  const stats = [
    fmt(n.cost) && `Cost: ${fmt(n.cost)}`,
    fmt(n.bytes) && `Bytes: ${fmt(n.bytes)}`,
    fmt(n.cardinality) && `Cardinality: ${fmt(n.cardinality)}`,
    n.time && `Time: ${n.time}`,
  ].filter(Boolean);
  return (
    <>
      <div
        className={clsx("flex items-start gap-1 border-b border-line/40 py-1 pr-3", isRoot && "bg-warn/15")}
        style={{ paddingLeft: 8 + n.depth * 22 }}
      >
        <span className="w-6 shrink-0 pt-0.5 text-right font-mono text-[11px] font-semibold text-muted">{isRoot ? "" : index}</span>
        <button className="mt-0.5 shrink-0 text-muted hover:text-fg" onClick={() => toggle(n.id)} disabled={!n.children.length}>
          {n.children.length ? open ? <ChevronDown size={14} /> : <ChevronRight size={14} /> : <span className="inline-block w-3.5" />}
        </button>
        <span className={clsx("mt-1.5 h-2.5 w-2.5 shrink-0 rounded-full", t.dot)} />
        <div className="min-w-0">
          <div className={clsx("font-semibold", isRoot ? "text-accent" : t.text)}>
            {n.operation} {n.options}
            {n.object && <span className="ml-1.5 font-normal text-fg">{n.object}</span>}
            {n.optimizer && isRoot && <span className="ml-2 font-normal text-muted">{n.optimizer}</span>}
          </div>
          {stats.length > 0 && <div className="text-[11px] text-muted">{stats.join("  ")}</div>}
          {n.accessPredicates && (
            <div className="font-mono text-[11px] text-ok">access: {n.accessPredicates}</div>
          )}
          {n.filterPredicates && (
            <div className="font-mono text-[11px] text-warn">filter: {n.filterPredicates}</div>
          )}
        </div>
      </div>
      {open && n.children.map((c) => <RowWithIndex key={c.id} n={c} collapsed={collapsed} toggle={toggle} />)}
    </>
  );
}

function RowWithIndex(props: { n: TreeNode; collapsed: Set<number>; toggle: (id: number) => void }) {
  return <Row {...props} index={props.n.id} />;
}

export function ExplainPlanView({ plan, sql }: { plan: Plan; sql: string }) {
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const [mode, setMode] = useState<"tree" | "text">("tree");
  const roots = useMemo(() => build(plan.nodes), [plan]);
  const toggle = (id: number) =>
    setCollapsed((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  return (
    <div className="flex h-full flex-col bg-panel">
      <div className="flex items-center gap-1 border-b border-line px-2 py-1">
        <span className="px-1 text-xs font-semibold">Explain Plan</span>
        <span className="max-w-[40%] truncate font-mono text-[11px] text-muted" title={sql}>
          {sql}
        </span>
        <div className="flex-1" />
        <Button size="sm" variant={mode === "tree" ? "default" : "ghost"} onClick={() => setMode("tree")}>
          <Network size={13} /> Tree
        </Button>
        {plan.text && (
          <Button size="sm" variant={mode === "text" ? "default" : "ghost"} onClick={() => setMode("text")}>
            <FileText size={13} /> Text
          </Button>
        )}
        <Button size="sm" variant="ghost" onClick={() => navigator.clipboard.writeText(plan.text || plan.nodes.map((n) => `${n.operation} ${n.options} ${n.object}`).join("\n"))}>
          <Copy size={13} /> Copy
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto text-[13px] select-text">
        {mode === "tree" ? (
          roots.map((r) => <RowWithIndex key={r.id} n={r} collapsed={collapsed} toggle={toggle} />)
        ) : (
          <pre className="p-3 font-mono text-xs">{plan.text}</pre>
        )}
      </div>
    </div>
  );
}
