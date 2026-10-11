import cytoscape, { type Core, type ElementDefinition } from "cytoscape";
import { useEffect, useMemo, useRef, useState } from "react";
import type { CellValue, QueryResult } from "../../lib/ipc";
import { useThemeColors } from "../../lib/theme";

type Json = Record<string, unknown>;

const EDGE_TIMEOUT_MS = 8000;
const PALETTE = ["#2563eb", "#16a34a", "#dc2626", "#9333ea", "#ea580c", "#0891b2", "#ca8a04", "#db2777", "#4b5563", "#65a30d"];

/** Blends `hex` toward `base` (`t` = share of `hex`). */
function mix(hex: string, base: string, t: number): string {
  const c = (h: string, i: number) => parseInt(h.slice(1 + i * 2, 3 + i * 2), 16);
  const ch = (i: number) => Math.round(c(hex, i) * t + c(base, i) * (1 - t)).toString(16).padStart(2, "0");
  return `#${ch(0)}${ch(1)}${ch(2)}`;
}

/** Gives every edge a gentle bend; parallel edges between the same pair fan out on alternate sides. */
function curved(els: ElementDefinition[]): ElementDefinition[] {
  const seen = new Map<string, number>();
  return els.map((e) => {
    const { source, target } = e.data as { source?: string; target?: string };
    if (source == null || target == null) return e;
    if (source === target) return { ...e, data: { ...e.data, bend: 0 } };
    const key = source < target ? `${source}|${target}` : `${target}|${source}`;
    const k = seen.get(key) ?? 0;
    seen.set(key, k + 1);
    // Distances are relative to source→target, so A→B and B→A already bow to opposite sides.
    const bend = (k % 2 ? -1 : 1) * (22 + 28 * Math.floor(k / 2));
    return { ...e, data: { ...e.data, bend } };
  });
}

function parse(v: CellValue): Json | null {
  if (typeof v !== "string" || !v.startsWith("{") || !v.includes('"~')) return null;
  try {
    return JSON.parse(v) as Json;
  } catch {
    return null;
  }
}

/** True when a result holds Neo4j / JanusGraph nodes / relationships / paths. */
export function hasGraph(r: QueryResult): boolean {
  for (const row of r.rows.slice(0, 20)) for (const c of row) if (parse(c)) return true;
  return false;
}

function props(o: Json): Json {
  return Object.fromEntries(Object.entries(o).filter(([k]) => !k.startsWith("~")));
}

function caption(o: Json): string {
  const p = props(o);
  for (const k of ["name", "title", "label", "id", "code", "email"]) if (p[k] != null) return String(p[k]);
  const first = Object.values(p)[0];
  return first != null ? String(first) : String(o["~id"]);
}

export function buildElements(r: QueryResult, extraEdges: Json[] = []): { els: ElementDefinition[]; labels: string[] } {
  const nodes = new Map<string, ElementDefinition>();
  const edges = new Map<string, ElementDefinition>();
  const labels: string[] = [];
  const addNode = (o: Json) => {
    const id = `n${o["~id"]}`;
    const ls = (o["~labels"] as string[]) ?? [];
    const label = ls[ls.length - 1] ?? "";
    if (label && !labels.includes(label)) labels.push(label);
    nodes.set(id, { data: { id, caption: caption(o), label, props: props(o), labels: o["~labels"] } });
  };
  const addRel = (o: Json) => {
    if (o["~start"] == null) return;
    const id = `r${o["~id"]}`;
    edges.set(id, { data: { id, source: `n${o["~start"]}`, target: `n${o["~end"]}`, caption: String(o["~type"]), props: props(o) } });
  };
  const visit = (o: Json) => {
    if (Array.isArray(o["~path"])) {
      (o["~path"] as Json[]).forEach(visit);
      // Neo4j path rels are unbounded (no start/end): connect consecutive nodes.
      // Gremlin path edges keep their own direction.
      const ns = o["~path"] as Json[];
      const rs = (o["~rels"] as Json[]) ?? [];
      rs.forEach((rel, i) => {
        if (rel["~start"] != null) addRel(rel);
        else if (ns[i] && ns[i + 1]) addRel({ ...rel, "~start": ns[i]["~id"], "~end": ns[i + 1]["~id"] });
      });
    } else if (o["~labels"]) addNode(o);
    else if (o["~type"]) addRel(o);
  };
  for (const row of r.rows) for (const c of row) {
    const o = parse(c);
    if (o) visit(o);
  }
  extraEdges.forEach(addRel);
  // Drop edges whose endpoints were not returned.
  const els = [...nodes.values(), ...[...edges.values()].filter((e) => nodes.has(e.data.source as string) && nodes.has(e.data.target as string))];
  return { els, labels };
}

interface Props {
  result: QueryResult;
  /** Loads the edges between the given node ids, so a node-only result (`g.V()`, `MATCH (n)`) still shows relationships. */
  fetchEdges?: (ids: string[]) => Promise<Json[]>;
}

export function GraphView({ result, fetchEdges }: Props) {
  const colors = useThemeColors();
  const ref = useRef<HTMLDivElement>(null);
  const cy = useRef<Core | null>(null);
  const [selected, setSelected] = useState<{
    caption: string;
    kind: string;
    color: string;
    id: string;
    degree?: { out: number; in: number };
    props: Json;
  } | null>(null);
  // null while the connecting edges are loading (the first layout waits for them).
  const [extra, setExtra] = useState<Json[] | null>(fetchEdges ? null : []);
  const nodeIds = useMemo(
    () => buildElements(result).els.filter((e) => e.data.source == null).map((e) => String(e.data.id).slice(1)),
    [result],
  );
  useEffect(() => {
    if (!fetchEdges || nodeIds.length < 2) return setExtra([]);
    let alive = true;
    setExtra(null);
    // A slow / hung edge lookup must not keep the graph hidden: draw the nodes alone after a while.
    const giveUp = setTimeout(() => alive && setExtra((cur) => cur ?? []), EDGE_TIMEOUT_MS);
    fetchEdges(nodeIds)
      .then((es) => alive && setExtra(es))
      .catch(() => alive && setExtra((cur) => cur ?? []))
      .finally(() => clearTimeout(giveUp));
    return () => {
      alive = false;
      clearTimeout(giveUp);
    };
  }, [nodeIds, fetchEdges]);
  const { els, labels } = useMemo(() => buildElements(result, extra ?? []), [result, extra]);
  const counts = useMemo(() => {
    const m = new Map<string, number>();
    for (const e of els) if (e.data.source == null) m.set(e.data.label, (m.get(e.data.label) ?? 0) + 1);
    return m;
  }, [els]);

  useEffect(() => {
    if (!ref.current || extra === null) return;
    const color = (l: string) => PALETTE[Math.max(0, labels.indexOf(l)) % PALETTE.length];
    const base = colors.dark ? "#111827" : "#ffffff";
    const fill = (l: string) => mix(color(l), base, colors.dark ? 0.55 : 0.4);
    const edgeColor = colors.dark ? "#fbbf24" : "#f59e0b";
    const font = "Inter, ui-sans-serif, system-ui, sans-serif";
    const inst = cytoscape({
      container: ref.current,
      elements: curved(els),
      wheelSensitivity: 0.3,
      maxZoom: 2.5,
      minZoom: 0.05,
      style: [
        {
          selector: "node",
          style: {
            "background-color": (n: cytoscape.NodeSingular) => fill(n.data("label")),
            "border-color": (n: cytoscape.NodeSingular) => color(n.data("label")),
            "border-width": 2.5,
            width: 38,
            height: 38,
            label: "data(caption)",
            color: colors.fg,
            "font-family": font,
            "font-size": 11,
            "font-weight": 500,
            "text-valign": "bottom",
            "text-margin-y": 6,
            "text-outline-color": colors.panel,
            "text-outline-width": 2.5,
            "underlay-color": (n: cytoscape.NodeSingular) => color(n.data("label")),
            "underlay-opacity": 0,
            "underlay-padding": 7,
            "underlay-shape": "ellipse",
          },
        },
        {
          selector: "edge",
          style: {
            width: 1.8,
            "line-color": edgeColor,
            "target-arrow-color": edgeColor,
            "target-arrow-shape": "triangle",
            "arrow-scale": 1.1,
            "curve-style": "unbundled-bezier",
            "control-point-distances": "data(bend)",
            "control-point-weights": 0.5,
            label: "data(caption)",
            "font-family": font,
            "font-size": 10,
            color: colors.fg,
            "text-opacity": 0.75,
            "text-rotation": "autorotate",
            "text-outline-color": colors.panel,
            "text-outline-width": 3,
          },
        },
        { selector: "node:active, node.hover", style: { "underlay-opacity": 0.18 } },
        { selector: "node:selected", style: { "underlay-opacity": 0.3, "border-width": 3.5, "font-weight": 700 } },
        { selector: "edge.hover, edge.lit", style: { width: 3, "text-opacity": 1, "font-weight": 700 } },
        { selector: "edge:selected", style: { width: 3.5, "text-opacity": 1, "font-weight": 700 } },
        { selector: ".faded", style: { opacity: 0.4 } },
      ],
      layout: { name: "preset" },
    });
    // Selecting an element spotlights it with its direct neighbourhood; the rest fades.
    const spotlight = (t: cytoscape.SingularElementArgument | null) => {
      inst.batch(() => {
        inst.elements().removeClass("faded lit");
        if (!t) return;
        const keep = t.isNode() ? t.closedNeighborhood() : t.union(t.connectedNodes());
        inst.elements().not(keep).addClass("faded");
        if (t.isNode()) t.connectedEdges().addClass("lit");
      });
    };
    inst.on("mouseover", "node, edge", (e) => {
      e.target.addClass("hover");
      ref.current?.style.setProperty("cursor", "pointer");
    });
    inst.on("mouseout", "node, edge", (e) => {
      e.target.removeClass("hover");
      ref.current?.style.removeProperty("cursor");
    });
    inst.on("tap", "node, edge", (e) => {
      const d = e.target.data();
      spotlight(e.target);
      setSelected({
        caption: d.caption,
        kind: e.target.isNode() ? `:${(d.labels ?? []).join(":")}` : `[:${d.caption}]`,
        color: e.target.isNode() ? color(d.label) : edgeColor,
        id: String(d.id).slice(1),
        degree: e.target.isNode() ? { out: e.target.outgoers("edge").length, in: e.target.incomers("edge").length } : undefined,
        props: d.props,
      });
    });
    inst.on("tap", (e) => {
      if (e.target !== inst) return;
      spotlight(null);
      setSelected(null);
    });
    cy.current = inst;
    let laidOut = false;
    let alive = true;
    let size = { w: 0, h: 0 };
    const relayout = () => {
      if (!alive) return;
      inst.resize();
      const w = inst.width(), h = inst.height();
      if (w === 0 || h === 0) return;
      const prev = size;
      size = { w, h };
      if (!laidOut) {
        laidOut = true;
        const opts = {
          name: els.length > 400 ? "grid" : "cose",
          animate: false,
          padding: 40,
          nodeRepulsion: () => 60000,
          idealEdgeLength: () => 130,
          edgeElasticity: () => 80,
          gravity: 0.6,
          numIter: 2500,
          componentSpacing: 120,
        };
        inst.layout(opts as cytoscape.LayoutOptions).run();
      } else {
        // Keep the user's zoom; just keep the view centred (e.g. when the detail panel opens).
        inst.panBy({ x: (w - prev.w) / 2, y: (h - prev.h) / 2 });
      }
    };
    requestAnimationFrame(relayout);
    const ro = new ResizeObserver(relayout);
    ro.observe(ref.current);
    return () => {
      alive = false;
      ro.disconnect();
      inst.destroy();
    };
  }, [els, labels, colors, extra]);

  return (
    <div className="absolute inset-0 flex">
      <div className="relative min-w-0 flex-1 bg-panel">
        <div className="absolute inset-0">
          <div ref={ref} className="h-full w-full" />
        </div>
        <div className="absolute left-2 top-2 flex flex-wrap gap-1">
          {labels.map((l, i) => {
            const c = PALETTE[i % PALETTE.length];
            return (
              <span
                key={l}
                className="flex items-center gap-1.5 rounded-full border bg-panel/90 px-2 py-0.5 text-[11px] font-medium shadow-sm backdrop-blur"
                style={{ borderColor: c }}
              >
                <span className="h-2 w-2 rounded-full" style={{ background: c }} />
                {l}
                <span className="text-muted">{counts.get(l) ?? 0}</span>
              </span>
            );
          })}
        </div>
        {els.length > 0 && (
          <button
            title="Zoom to fit"
            className="absolute bottom-2 right-2 rounded-md border border-line bg-panel/90 px-2 py-1 text-[11px] text-muted shadow-sm hover:text-fg"
            onClick={() => cy.current?.animate({ fit: { eles: cy.current.elements(), padding: 40 }, duration: 250 })}
          >
            Fit
          </button>
        )}
        {extra === null && <div className="absolute bottom-2 left-2 text-[11px] text-muted">Loading relationships…</div>}
        {els.length === 0 && <div className="p-6 text-muted">No nodes or relationships in this result.</div>}
      </div>
      {selected && (
        <div className="w-72 shrink-0 overflow-auto border-l border-line bg-panel text-xs select-text">
          <div className="border-b border-line p-3">
            <div className="flex items-center gap-2">
              <span className="h-3 w-3 shrink-0 rounded-full" style={{ background: selected.color }} />
              <span className="truncate text-sm font-semibold">{selected.caption}</span>
            </div>
            <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
              <span className="rounded px-1.5 py-0.5 font-mono text-[11px]" style={{ background: `${selected.color}22`, color: selected.color }}>
                {selected.kind}
              </span>
              <span className="font-mono text-muted">id {selected.id}</span>
            </div>
            {selected.degree && (
              <div className="mt-2 flex gap-3 text-muted">
                <span>→ {selected.degree.out} out</span>
                <span>← {selected.degree.in} in</span>
              </div>
            )}
          </div>
          <table className="w-full">
            <tbody>
              {Object.entries(selected.props).map(([k, v]) => (
                <tr key={k} className="border-b border-line/60 hover:bg-hover">
                  <td className="w-24 py-1.5 pl-3 pr-2 align-top font-medium text-muted">{k}</td>
                  <td className="break-all py-1.5 pr-3 font-mono">{typeof v === "object" ? JSON.stringify(v) : String(v)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {Object.keys(selected.props).length === 0 && <div className="p-3 text-muted">No properties.</div>}
        </div>
      )}
    </div>
  );
}
