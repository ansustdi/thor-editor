import { open as openDialog } from "@tauri-apps/plugin-dialog";
import clsx from "clsx";
import { FolderOpen } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { Button, Checkbox, Field, Input, Modal, Select, Spinner } from "../../components/ui";
import { api, errorText, onImportProgress, type ColumnInfo, type ImportPreview, type ImportSource } from "../../lib/ipc";
import { qualified, quoteIdent } from "../../lib/sql";
import { uid, useApp, type Connection } from "../../store/app";

interface Props {
  conn: Connection;
  schema: string;
  table?: string;
  onClose: () => void;
}

const SKIP = "\u0000skip";

export function ImportDialog({ conn, schema, table, onClose }: Props) {
  const d = conn.info.dialect;
  const { objects, cacheObjects } = useApp();
  const tables = (objects[conn.key]?.[schema] ?? []).filter((o) => o.kind === "table").map((o) => o.name);
  const [path, setPath] = useState("");
  const [src, setSrc] = useState<ImportSource>({ format: "", delimiter: ",", header: true, sheet: "" });
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [mode, setMode] = useState<"existing" | "new">(table ? "existing" : "new");
  const [target, setTarget] = useState(table ?? "");
  const [newName, setNewName] = useState("");
  const [targetCols, setTargetCols] = useState<ColumnInfo[]>([]);
  const [mapping, setMapping] = useState<string[]>([]);
  const [types, setTypes] = useState<string[]>([]);
  const [truncate, setTruncate] = useState(false);
  const [emptyAsNull, setEmptyAsNull] = useState(true);
  const [batch, setBatch] = useState(500);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [jobId] = useState(() => uid("i"));

  useEffect(() => {
    const un = onImportProgress((p) => p.jobId === jobId && setProgress(p.rows));
    return () => {
      un.then((f) => f());
    };
  }, [jobId]);

  const loadPreview = async (p: string, s: ImportSource) => {
    setMsg(null);
    try {
      const pv = await api.importPreview(conn.info.sessionId, p, s);
      setPreview(pv);
      setTypes(pv.types);
      if (!newName) setNewName(p.split(/[\\/]/).pop()!.replace(/\.[^.]+$/, "").replace(/\W+/g, "_").toLowerCase());
    } catch (e) {
      setPreview(null);
      setMsg({ ok: false, text: errorText(e) });
    }
  };

  const pick = async () => {
    const f = await openDialog({
      filters: [
        { name: "Data files", extensions: ["csv", "tsv", "txt", "xlsx", "xls", "xlsm", "ods"] },
        { name: "All", extensions: ["*"] },
      ],
    });
    if (typeof f !== "string") return;
    setPath(f);
    const s = { ...src, sheet: "" };
    setSrc(s);
    loadPreview(f, s);
  };

  useEffect(() => {
    if (mode !== "existing" || !target) {
      setTargetCols([]);
      return;
    }
    api.tableColumns(conn.info.sessionId, schema, target).then(setTargetCols).catch(() => setTargetCols([]));
  }, [mode, target, schema, conn.info.sessionId]);

  // Auto-map by (case-insensitive) name.
  useEffect(() => {
    if (!preview) return;
    if (mode === "new") {
      setMapping(preview.columns.map((c) => c.replace(/\W+/g, "_")));
    } else {
      setMapping(preview.columns.map((c) => targetCols.find((t) => t.name.toLowerCase() === c.toLowerCase().replace(/\W+/g, "_") || t.name.toLowerCase() === c.toLowerCase())?.name ?? SKIP));
    }
  }, [preview, mode, targetCols]);

  const mapped = useMemo(() => mapping.filter((m) => m && m !== SKIP).length, [mapping]);
  const tableName = mode === "new" ? newName : target;

  const run = async () => {
    if (!preview || !tableName) return;
    setBusy(true);
    setMsg(null);
    setProgress(0);
    try {
      const n = await api.importFile(
        conn.info.sessionId,
        path,
        src,
        {
          table: qualified(d, schema, tableName),
          mapping: mapping.map((m) => (!m || m === SKIP ? "" : quoteIdent(d, m))),
          create: mode === "new",
          types,
          truncate: mode === "existing" && truncate,
          emptyAsNull,
          batchSize: batch,
        },
        jobId,
      );
      const commitNote = conn.autocommit ? "" : " Auto-commit is OFF — commit in a SQL tab to keep the rows.";
      setMsg({ ok: true, text: `Imported ${n.toLocaleString()} rows into ${tableName}.${commitNote}` });
      if (mode === "new") cacheObjects(conn.key, schema, await api.listObjects(conn.info.sessionId, schema));
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  };

  const isExcel = /\.(xlsx|xls|xlsm|ods)$/i.test(path);

  return (
    <Modal
      title={`Import data into ${schema}`}
      onClose={busy ? () => {} : onClose}
      width={900}
      footer={
        <>
          {busy && (
            <span className="mr-auto inline-flex items-center gap-2 text-xs text-muted">
              <Spinner size={12} /> {progress.toLocaleString()} rows…
            </span>
          )}
          <Button onClick={onClose} disabled={busy}>
            Close
          </Button>
          <Button variant="primary" onClick={run} disabled={busy || !preview || !tableName || !mapped}>
            Import
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="flex items-end gap-2">
          <Field label="File" className="flex-1">
            <Input value={path} readOnly placeholder="Choose a CSV, TSV or Excel file…" onClick={pick} />
          </Field>
          <Button onClick={pick}>
            <FolderOpen size={14} /> Browse
          </Button>
        </div>

        {path && (
          <div className="flex flex-wrap items-end gap-3">
            {!isExcel && (
              <Field label="Delimiter" className="w-36">
                <Select
                  value={src.delimiter}
                  onChange={(e) => {
                    const s = { ...src, delimiter: e.target.value, format: e.target.value === "\t" ? "tsv" : "" };
                    setSrc(s);
                    loadPreview(path, s);
                  }}
                >
                  <option value=",">Comma</option>
                  <option value=";">Semicolon</option>
                  <option value={"\t"}>Tab</option>
                  <option value="|">Pipe</option>
                </Select>
              </Field>
            )}
            {isExcel && preview && preview.sheets.length > 0 && (
              <Field label="Sheet" className="w-44">
                <Select
                  value={src.sheet || preview.sheets[0]}
                  onChange={(e) => {
                    const s = { ...src, sheet: e.target.value };
                    setSrc(s);
                    loadPreview(path, s);
                  }}
                >
                  {preview.sheets.map((sh) => (
                    <option key={sh}>{sh}</option>
                  ))}
                </Select>
              </Field>
            )}
            <Checkbox
              label="First row is header"
              checked={src.header}
              onChange={(v) => {
                const s = { ...src, header: v };
                setSrc(s);
                loadPreview(path, s);
              }}
            />
            <Checkbox label="Empty cell = NULL" checked={emptyAsNull} onChange={setEmptyAsNull} />
            <Field label="Rows per INSERT" className="w-32">
              <Input type="number" min={1} value={batch} onChange={(e) => setBatch(Math.max(1, Number(e.target.value) || 1))} />
            </Field>
          </div>
        )}

        {preview && (
          <>
            <div className="flex flex-wrap items-end gap-3 rounded-md border border-line p-3">
              <label className="inline-flex items-center gap-1.5 text-[13px]">
                <input type="radio" className="accent-[var(--accent)]" checked={mode === "existing"} onChange={() => setMode("existing")} />
                Existing table
              </label>
              <Select className="w-56" value={target} onChange={(e) => setTarget(e.target.value)} disabled={mode !== "existing"}>
                <option value="">— choose —</option>
                {tables.map((t) => (
                  <option key={t}>{t}</option>
                ))}
              </Select>
              {mode === "existing" && <Checkbox label="Empty table first" checked={truncate} onChange={setTruncate} />}
              <span className="mx-2 h-6 w-px bg-line" />
              <label className="inline-flex items-center gap-1.5 text-[13px]">
                <input type="radio" className="accent-[var(--accent)]" checked={mode === "new"} onChange={() => setMode("new")} />
                New table
              </label>
              <Input className="w-56" value={newName} onChange={(e) => setNewName(e.target.value)} disabled={mode !== "new"} />
            </div>

            <div className="max-h-80 overflow-auto rounded-md border border-line">
              <table className="text-xs">
                <thead className="sticky top-0 z-10 bg-panel-2">
                  <tr>
                    {preview.columns.map((c, i) => (
                      <th key={i} className="min-w-36 border-b border-r border-line p-1.5 text-left align-top font-normal">
                        <div className="mb-1 font-semibold">{c}</div>
                        {mode === "existing" ? (
                          <Select
                            className={clsx("h-7 text-xs", mapping[i] === SKIP && "text-muted")}
                            value={mapping[i] ?? SKIP}
                            onChange={(e) => setMapping((m) => m.map((x, j) => (j === i ? e.target.value : x)))}
                          >
                            <option value={SKIP}>(skip)</option>
                            {targetCols.map((t) => (
                              <option key={t.name} value={t.name}>
                                → {t.name}
                              </option>
                            ))}
                          </Select>
                        ) : (
                          <div className="space-y-1">
                            <Input
                              className="h-7 text-xs"
                              value={mapping[i] ?? ""}
                              placeholder="(skip)"
                              onChange={(e) => setMapping((m) => m.map((x, j) => (j === i ? e.target.value : x)))}
                            />
                            <Input
                              className="h-7 font-mono text-[11px]"
                              value={types[i] ?? ""}
                              onChange={(e) => setTypes((t) => t.map((x, j) => (j === i ? e.target.value : x)))}
                            />
                          </div>
                        )}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody className="font-mono">
                  {preview.rows.slice(0, 50).map((r, i) => (
                    <tr key={i} className="border-b border-line/60">
                      {preview.columns.map((_, j) => (
                        <td key={j} className={clsx("max-w-60 truncate border-r border-line/60 px-1.5 py-0.5", mapping[j] === SKIP && "opacity-40")}>
                          {r[j] ?? ""}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="text-xs text-muted">
              {mapped} of {preview.columns.length} columns mapped · preview shows the first {Math.min(50, preview.rows.length)} rows
            </div>
          </>
        )}

        {msg && (
          <div className={clsx("rounded-md px-3 py-2 text-xs select-text", msg.ok ? "bg-ok/10 text-ok" : "bg-danger/10 text-danger")}>{msg.text}</div>
        )}
      </div>
    </Modal>
  );
}
