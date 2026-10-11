import Editor, { type OnMount } from "@monaco-editor/react";
import clsx from "clsx";
import { AlertCircle, CheckCircle2, Copy, FileInput, Hammer, RefreshCw, Search, Undo2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type * as Monaco from "monaco-editor/editor/editor.api";
import { Button, Input, Spinner } from "../../components/ui";
import { api, errorText, type DbObject, type ObjectSource } from "../../lib/ipc";
import { monaco } from "../../lib/monaco";
import { useThemeColors } from "../../lib/theme";
import { editorTheme, useObjectHighlight } from "../../lib/editorTheme";
import { useApp, uid, type Connection, type SourceFocus } from "../../store/app";

export interface Member {
  kind: "procedure" | "function" | "cursor" | "type";
  name: string;
  returns?: string;
  line: number;
}

/** Procedures / functions / cursors / types declared in PL/SQL source (comments ignored). */
export function outline(text: string): Member[] {
  const clean = text
    .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, " "))
    .replace(/--[^\n]*/g, "")
    .replace(/'(?:[^']|'')*'/g, (m) => m.replace(/[^\n]/g, " "));
  const lines = clean.split("\n");
  const out: Member[] = [];
  const re = /^\s*(PROCEDURE|FUNCTION|CURSOR|TYPE)\s+("[^"]+"|[\w$#]+)/i;
  lines.forEach((l, i) => {
    const m = re.exec(l);
    if (!m) return;
    const kind = m[1].toLowerCase() as Member["kind"];
    const name = m[2].replace(/"/g, "");
    let returns: string | undefined;
    if (kind === "function") {
      const tail = lines.slice(i, i + 15).join(" ");
      returns = /\bRETURN\s+([\w$#.%]+)/i.exec(tail)?.[1]?.toUpperCase();
    }
    out.push({ kind, name, returns, line: i + 1 });
  });
  return out;
}

const KIND_BADGE: Record<Member["kind"], string> = { procedure: "P", function: "ƒ", cursor: "C", type: "T" };

export function MemberBadge({ kind }: { kind: Member["kind"] }) {
  return (
    <span
      className={clsx(
        "inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-bold text-white",
        kind === "function" ? "bg-accent" : kind === "procedure" ? "bg-ok" : "bg-muted",
      )}
    >
      {KIND_BADGE[kind]}
    </span>
  );
}

export function SourceView({ object, conn, focus }: { object: DbObject; conn: Connection; focus?: SourceFocus }) {
  const colors = useThemeColors();
  const { settings, openTab } = useApp();
  const [src, setSrc] = useState<ObjectSource | null>(null);
  const [active, setActive] = useState(0);
  const [edited, setEdited] = useState<Record<number, string>>({});
  const [loading, setLoading] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [filter, setFilter] = useState("");
  const edRef = useRef<Monaco.editor.IStandaloneCodeEditor | null>(null);
  const [edInst, setEdInst] = useState<Monaco.editor.IStandaloneCodeEditor | null>(null);
  useObjectHighlight(edInst, conn.key, settings.highlightObjects ?? true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const s = await api.objectSource(conn.info.sessionId, object.schema, object.name, object.kind);
      setSrc(s);
      setEdited({});
      setActive((a) => Math.min(a, Math.max(0, s.parts.length - 1)));
      return s;
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
      return null;
    } finally {
      setLoading(false);
    }
  }, [conn.info.sessionId, object]);

  useEffect(() => {
    load();
  }, [load]);

  const part = src?.parts[active];
  const text = edited[active] ?? part?.text ?? "";
  const dirty = edited[active] !== undefined && edited[active] !== part?.text;
  const members = useMemo(() => outline(text), [text]);
  const errors = useMemo(() => (src?.errors ?? []).filter((e) => !part || e.objectType === part.objectType), [src, part]);

  // Compile errors as editor markers.
  useEffect(() => {
    const model = edRef.current?.getModel();
    if (!model) return;
    monaco.editor.setModelMarkers(
      model,
      "oracle",
      errors.map((e) => ({
        startLineNumber: e.line,
        startColumn: Math.max(1, e.position),
        endLineNumber: e.line,
        endColumn: model.getLineMaxColumn(Math.min(e.line, model.getLineCount())),
        message: e.text,
        severity: e.attribute === "WARNING" ? monaco.MarkerSeverity.Warning : monaco.MarkerSeverity.Error,
      })),
    );
  }, [errors, text]);

  // Explorer tree navigation: select the requested part, then jump once its editor is mounted.
  const edPart = useRef(-1);
  const appliedSeq = useRef(-1);
  useEffect(() => {
    if (!focus || !src || appliedSeq.current === focus.seq) return;
    const idx = src.parts.findIndex((p) => p.label === focus.part);
    if (idx < 0) {
      appliedSeq.current = focus.seq;
      return;
    }
    if (idx !== active) {
      setActive(idx);
      return;
    }
    if (!edInst || edPart.current !== idx) return;
    appliedSeq.current = focus.seq;
    if (focus.line) goto(focus.line);
    else edInst.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focus?.seq, src, active, edInst]);

  const goto = (line: number, col = 1) => {
    const ed = edRef.current;
    if (!ed) return;
    ed.revealLineInCenter(line);
    ed.setPosition({ lineNumber: line, column: col });
    ed.focus();
  };

  const compile = async () => {
    if (!part) return;
    setLoading(true);
    setMsg(null);
    try {
      // Oracle reports compile errors as success-with-info; the real result is in ALL_ERRORS.
      await api.execute(conn.info.sessionId, text, 0);
      const s = await load();
      const p = s?.parts.find((x) => x.objectType === part.objectType);
      const n = s?.errors.filter((e) => e.objectType === part.objectType).length ?? 0;
      setMsg(
        n === 0 && p?.status !== "INVALID"
          ? { ok: true, text: `${part.label} compiled successfully.` }
          : { ok: false, text: `${part.label} compiled with ${n} error(s).` },
      );
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
      setLoading(false);
    }
  };

  const onMount: OnMount = (ed) => {
    edRef.current = ed;
    edPart.current = active;
    setEdInst(ed);
    // addAction is scoped to this editor (addCommand would be shared across all editors).
    ed.addAction({ id: "thor.compile", label: "Compile", keybindings: [monaco.KeyCode.F9], run: () => compileRef.current() });
  };
  const compileRef = useRef(compile);
  compileRef.current = compile;

  const shown = members.filter((m) => !filter || m.name.toLowerCase().includes(filter.toLowerCase()));
  const statusCls = (st: string) => (st === "VALID" ? "text-ok" : st ? "text-danger" : "");
  const title = (s: string) => s.charAt(0) + s.slice(1).toLowerCase();

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Toad-style status lines */}
      <div className="border-b border-line bg-panel px-3 py-1.5 font-mono text-[11px] leading-5">
        {(src?.parts ?? []).map((p) => (
          <div key={p.objectType || p.label}>
            <b>
              {object.name} ({p.label})
            </b>
            {p.created && <> · Created: {p.created}</>}
            {p.lastDdl && <> · Last DDL: {p.lastDdl}</>}
            {p.status && (
              <>
                {" "}
                · Status: <span className={clsx("font-semibold", statusCls(p.status))}>{title(p.status)}</span>
              </>
            )}
          </div>
        ))}
      </div>

      <div className="flex items-center gap-1 border-b border-line bg-panel px-2 py-1">
        {(src?.parts ?? []).map((p, i) => (
          <button
            key={p.label}
            onClick={() => setActive(i)}
            className={clsx(
              "h-7 rounded-md px-3 text-xs font-medium",
              i === active ? "bg-accent-soft text-accent" : "text-muted hover:bg-hover",
            )}
          >
            {p.label}
            {edited[i] !== undefined && edited[i] !== p.text && " •"}
            {p.status === "INVALID" && <AlertCircle size={11} className="ml-1 inline text-danger" />}
          </button>
        ))}
        <div className="mx-1 h-5 w-px bg-line" />
        {part?.objectType && (
          <Button size="sm" variant={dirty ? "primary" : "ghost"} onClick={compile} disabled={loading} title="Compile (F9)">
            <Hammer size={13} /> Compile
          </Button>
        )}
        {dirty && (
          <Button size="sm" variant="ghost" onClick={() => setEdited((e) => ({ ...e, [active]: part!.text }))}>
            <Undo2 size={13} /> Revert
          </Button>
        )}
        <Button size="sm" variant="ghost" onClick={() => navigator.clipboard.writeText(text)}>
          <Copy size={13} /> Copy
        </Button>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => openTab({ id: uid(), connKey: conn.key, kind: "sql", title: `${object.name} ${part?.label ?? ""}`, sql: text + "\n/" })}
        >
          <FileInput size={13} /> Open in editor
        </Button>
        <Button size="sm" variant="ghost" onClick={load} disabled={loading}>
          <RefreshCw size={13} /> Refresh
        </Button>
        {loading && <Spinner size={12} />}
        {msg && (
          <span className={clsx("ml-2 inline-flex items-center gap-1 text-xs", msg.ok ? "text-ok" : "text-danger")}>
            {msg.ok ? <CheckCircle2 size={13} /> : <AlertCircle size={13} />} {msg.text}
          </span>
        )}
      </div>

      <div className="flex min-h-0 flex-1">
        {/* Member outline */}
        <div className="flex w-60 shrink-0 flex-col border-r border-line bg-panel">
          <div className="relative p-1.5">
            <Search size={12} className="absolute left-3.5 top-3.5 text-muted" />
            <Input className="h-7 pl-6 text-xs" placeholder="Members…" value={filter} onChange={(e) => setFilter(e.target.value)} />
          </div>
          <div className="min-h-0 flex-1 overflow-auto pb-2 text-xs">
            {shown.length === 0 && <div className="px-3 py-2 text-muted">No procedures or functions.</div>}
            {shown.map((m, i) => (
              <button
                key={`${m.name}${m.line}${i}`}
                className="flex w-full items-center gap-1.5 px-2 py-[3px] text-left hover:bg-hover"
                onClick={() => goto(m.line)}
                title={`Line ${m.line}`}
              >
                <MemberBadge kind={m.kind} />
                <span className="truncate">
                  {m.name}
                  {m.returns && <span className="text-muted">: {m.returns}</span>}
                </span>
              </button>
            ))}
          </div>
        </div>

        <div className="flex min-w-0 flex-1 flex-col">
          <div className="min-h-0 flex-1">
            <Editor
              key={`${object.name}-${active}`}
              language="sql"
              value={text}
              onChange={(v) => setEdited((e) => ({ ...e, [active]: v ?? "" }))}
              onMount={onMount}
              theme={editorTheme(colors.dark, settings.editorScheme)}
              options={{
                  // Toad shows brackets black; Monaco colours bracket pairs by default.
                  bracketPairColorization: { enabled: settings.editorScheme === "vscode" },
                fontSize: settings.fontSize,
                fontFamily: '"Cascadia Code", Consolas, monospace',
                minimap: { enabled: true, scale: 1, renderCharacters: false },
                automaticLayout: true,
                scrollBeyondLastLine: false,
                folding: true,
                glyphMargin: true,
                renderLineHighlight: "all",
              }}
            />
          </div>
          {errors.length > 0 && (
            <div className="max-h-40 overflow-auto border-t border-line bg-panel text-xs">
              <div className="sticky top-0 bg-panel-2 px-3 py-1 font-semibold text-danger">{errors.length} compile error(s)</div>
              {errors.map((e, i) => (
                <button
                  key={i}
                  className="flex w-full gap-3 px-3 py-1 text-left font-mono hover:bg-hover"
                  onClick={() => goto(e.line, e.position)}
                >
                  <span className="w-16 shrink-0 text-muted">
                    {e.line}:{e.position}
                  </span>
                  <span className={e.attribute === "WARNING" ? "text-warn" : "text-danger"}>{e.text}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
