import type * as Monaco from "monaco-editor/editor/editor.api";
import { api, type DbObject } from "../../lib/ipc";
import { useApp, type Connection } from "../../store/app";
import { aliasMap } from "./completion";

const PREFERRED = ["table", "view", "materialized view", "package", "procedure", "function", "type", "sequence", "trigger", "collection", "label"];

/** Identifier (optionally schema-qualified / quoted) under a Monaco position. */
export function identifierAt(model: Monaco.editor.ITextModel, pos: Monaco.Position): { text: string; range: Monaco.IRange } | null {
  const line = model.getLineContent(pos.lineNumber);
  const re = /(?:"[^"]+"|[A-Za-z_][\w$#]*)(?:\s*\.\s*(?:"[^"]+"|[A-Za-z_][\w$#]*))*/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(line))) {
    const start = m.index + 1;
    const end = start + m[0].length;
    if (pos.column >= start && pos.column <= end) {
      return { text: m[0], range: { startLineNumber: pos.lineNumber, endLineNumber: pos.lineNumber, startColumn: start, endColumn: end } };
    }
  }
  return null;
}

const KEYWORD = /^(select|from|where|and|or|not|in|is|null|join|left|right|inner|outer|on|as|by|group|order|having|insert|into|update|delete|set|values|with|union|all|distinct|case|when|then|else|end|begin|declare|if|loop|for|return|cursor|type|procedure|function|package|body|exception|varchar2|number|date)$/i;

/** True for words that are never worth resolving as objects. */
export const isKeyword = (text: string) => KEYWORD.test(text);

/**
 * Toad-style navigation in a Monaco editor: Ctrl+hover underlines an identifier, Ctrl+click / F4 opens it.
 * Uses ed.addAction (scoped to this editor), not addCommand (shared by every editor).
 */
export function attachObjectNavigation(
  ed: Monaco.editor.IStandaloneCodeEditor,
  monaco: typeof Monaco,
  open: (text: string, line: number) => void,
): void {
  const link = ed.createDecorationsCollection();
  ed.onMouseMove((e) => {
    const pos = e.target.position;
    const hit = e.event.ctrlKey && pos ? identifierAt(ed.getModel()!, pos) : null;
    link.set(hit ? [{ range: hit.range, options: { inlineClassName: "ctrl-object-link" } }] : []);
  });
  ed.onKeyUp((e) => e.keyCode === monaco.KeyCode.Ctrl && link.clear());
  ed.onMouseDown((e) => {
    if (!e.event.ctrlKey || !e.target.position) return;
    const hit = identifierAt(ed.getModel()!, e.target.position);
    if (!hit) return;
    e.event.preventDefault();
    open(hit.text, e.target.position.lineNumber);
  });
  ed.addAction({
    id: "thor.openObject",
    label: "Open object at cursor",
    keybindings: [monaco.KeyCode.F4],
    run: () => {
      const pos = ed.getPosition()!;
      const hit = identifierAt(ed.getModel()!, pos);
      if (hit) open(hit.text, pos.lineNumber);
    },
  });
}

const unquote = (s: string) => (s.startsWith('"') ? s.slice(1, -1) : s);

/** Finds the database object an identifier refers to (alias → table, schema.name, synonyms). */
export async function resolveIdentifier(conn: Connection, text: string, sqlText: string, defaultSchema?: string): Promise<DbObject | null> {
  const st = useApp.getState();
  const d = conn.info.dialect;
  let parts = text.split(".").map((p) => unquote(p.trim()));
  // "alias.column" or "alias" → table behind the alias
  const alias = aliasMap(sqlText).get(parts[0].toLowerCase());
  if (alias && (parts.length === 1 || parts.length === 2)) {
    const aliased = alias.split(".").map(unquote);
    if (parts.length === 1 || aliased.length === 1 || alias.toLowerCase() !== parts.join(".").toLowerCase()) parts = aliased;
  }
  // package.member → the package
  const name = parts.length >= 2 ? parts[1] : parts[0];
  const home = defaultSchema ?? conn.info.currentSchema;
  const schema = parts.length >= 2 ? parts[0] : home;
  const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

  const lookIn = async (sch: string): Promise<DbObject | null> => {
    let list = st.objects[conn.key]?.[sch];
    if (!list) {
      try {
        list = await api.listObjects(conn.info.sessionId, sch);
        useApp.getState().cacheObjects(conn.key, sch, list);
      } catch {
        return null;
      }
    }
    const hits = list.filter((o) => eq(o.name, name));
    hits.sort((a, b) => (PREFERRED.indexOf(a.kind) + 99) % 99 - (PREFERRED.indexOf(b.kind) + 99) % 99);
    return hits.find((o) => o.kind !== "synonym") ?? null;
  };

  const schemaKey =
    Object.keys(st.objects[conn.key] ?? {}).find((s) => eq(s, schema)) ?? (d === "oracle" ? schema.toUpperCase() : schema);
  const local = await lookIn(schemaKey);
  if (local) return local;

  // qualifier might have been "package.member": try the first part as the object name
  if (parts.length === 2) {
    const asPkg = (st.objects[conn.key]?.[home] ?? []).find((o) => eq(o.name, parts[0]));
    if (asPkg) return asPkg;
  }

  if (d === "oracle") {
    const up = (s: string) => (/^[A-Za-z_][\w$#]*$/.test(s) ? s.toUpperCase() : s);
    const r = await api.resolveObject(conn.info.sessionId, up(schemaKey), up(name)).catch(() => null);
    if (r) return r;
    if (parts.length === 2) {
      return api.resolveObject(conn.info.sessionId, up(home), up(parts[0])).catch(() => null);
    }
  }
  return null;
}
