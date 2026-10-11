import type * as Monaco from "monaco-editor/editor/editor.api";
import { useEffect, useRef } from "react";
import { monaco } from "./monaco";
import { useApp } from "../store/app";
import { preloadReferencedSchemas } from "../features/editor/completion";

/**
 * Toad-like SQL colours: blue keywords, red strings, green comments, magenta built-ins
 * (SYSDATE, COUNT…), black numbers/operators — and object names (tables, views, packages…)
 * in olive via `useObjectHighlight`, which only marks names that exist in the database.
 */
monaco.editor.defineTheme("toad-light", {
  base: "vs",
  inherit: true,
  rules: [
    { token: "keyword", foreground: "0000FF" },
    { token: "operator", foreground: "0000FF" },
    // Monaco's base theme styles these per language (grey); override them explicitly.
    { token: "operator.sql", foreground: "0000FF" },
    { token: "operator.pgsql", foreground: "0000FF" },
    { token: "operator.mysql", foreground: "0000FF" },
    { token: "predefined", foreground: "FF00FF" },
    { token: "string", foreground: "FF0000" },
    { token: "string.double", foreground: "FF0000" },
    { token: "comment", foreground: "008000", fontStyle: "italic" },
    { token: "comment.quote", foreground: "008000", fontStyle: "italic" },
    { token: "number", foreground: "000000" },
    { token: "identifier", foreground: "000000" },
    { token: "identifier.quote", foreground: "000000" },
    { token: "delimiter", foreground: "000000" },
    { token: "type", foreground: "0000FF" },
  ],
  colors: {
    "editor.background": "#FFFFFF",
    "editor.lineHighlightBackground": "#E8F2FF",
    "editorLineNumber.foreground": "#2B91AF",
    "editor.selectionBackground": "#ADD6FF",
  },
});

monaco.editor.defineTheme("toad-dark", {
  base: "vs-dark",
  inherit: true,
  rules: [
    { token: "keyword", foreground: "569CD6" },
    { token: "operator", foreground: "569CD6" },
    { token: "operator.sql", foreground: "569CD6" },
    { token: "operator.pgsql", foreground: "569CD6" },
    { token: "operator.mysql", foreground: "569CD6" },
    { token: "predefined", foreground: "D670D6" },
    { token: "string", foreground: "F48771" },
    { token: "string.double", foreground: "F48771" },
    { token: "comment", foreground: "6A9955", fontStyle: "italic" },
    { token: "comment.quote", foreground: "6A9955", fontStyle: "italic" },
    { token: "number", foreground: "D4D4D4" },
    { token: "identifier", foreground: "D4D4D4" },
    { token: "delimiter", foreground: "D4D4D4" },
    { token: "type", foreground: "569CD6" },
  ],
  colors: {},
});

export type EditorScheme = "toad" | "vscode";

export function editorTheme(dark: boolean, scheme: EditorScheme | undefined): string {
  if ((scheme ?? "toad") === "vscode") return dark ? "vs-dark" : "vs";
  return dark ? "toad-dark" : "toad-light";
}

const IDENT = /"[^"\n]+"|`[^`\n]+`|[A-Za-z_][\w$#]*/g;

/** Offsets of strings and comments, which must never be decorated. */
function skipRanges(text: string): [number, number][] {
  const out: [number, number][] = [];
  const re = /'(?:[^']|'')*'|--[^\n]*|\/\*[\s\S]*?\*\//g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) out.push([m.index, m.index + m[0].length]);
  return out;
}

/**
 * Colours names of existing database objects (olive, like Toad). Matches `name` and `schema.name`
 * against the connection's cached object lists; re-runs on edits and when more objects load.
 */
export function useObjectHighlight(editor: Monaco.editor.IStandaloneCodeEditor | null, connKey: string, enabled = true) {
  const objects = useApp((s) => s.objects[connKey]);
  const deco = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);

  useEffect(() => {
    if (!editor) return;
    deco.current ??= editor.createDecorationsCollection();
    const coll = deco.current;
    if (!enabled) {
      coll.clear();
      return;
    }
    // name (lower-case) → kind; schema names too, so `wh.fact_sales` colours both parts
    const names = new Map<string, string>();
    const schemas = new Set<string>();
    for (const [schema, list] of Object.entries(objects ?? {})) {
      if (list.length) schemas.add(schema.toLowerCase());
      for (const o of list) if (!names.has(o.name.toLowerCase())) names.set(o.name.toLowerCase(), o.kind);
    }
    let timer: ReturnType<typeof setTimeout> | undefined;
    const paint = () => {
      const model = editor.getModel();
      if (!model) return;
      const text = model.getValue();
      if (text.length > 500_000) {
        coll.clear();
        return;
      }
      const skips = skipRanges(text);
      let si = 0;
      const decorations: Monaco.editor.IModelDeltaDecoration[] = [];
      IDENT.lastIndex = 0;
      let m: RegExpExecArray | null;
      while ((m = IDENT.exec(text))) {
        const at = m.index;
        while (si < skips.length && skips[si][1] <= at) si++;
        if (si < skips.length && skips[si][0] <= at) {
          IDENT.lastIndex = skips[si][1];
          continue;
        }
        const word = m[0].replace(/^["`]|["`]$/g, "").toLowerCase();
        const kind = names.get(word);
        const isSchema = !kind && schemas.has(word) && text[at + m[0].length] === ".";
        if (!kind && !isSchema) continue;
        const a = model.getPositionAt(at);
        const b = model.getPositionAt(at + m[0].length);
        decorations.push({
          range: { startLineNumber: a.lineNumber, startColumn: a.column, endLineNumber: b.lineNumber, endColumn: b.column },
          options: { inlineClassName: isSchema ? "toad-schema" : "toad-object" },
        });
      }
      coll.set(decorations);
      // `wh.fact_sales` with `wh` not loaded yet: fetch its objects, the store update repaints.
      const quals = [...text.matchAll(/([A-Za-z_][\w$#]*)\s*\.\s*[A-Za-z_"`]/g)].map((x) => x[1]).filter((q) => !schemas.has(q.toLowerCase()));
      const conn = useApp.getState().connections.find((c) => c.key === connKey);
      if (conn && quals.length) preloadReferencedSchemas(conn, quals.slice(0, 20)).catch(() => {});
    };
    paint();
    const sub = editor.onDidChangeModelContent(() => {
      clearTimeout(timer);
      timer = setTimeout(paint, 250);
    });
    return () => {
      clearTimeout(timer);
      sub.dispose();
    };
  }, [editor, objects, enabled]);
}
