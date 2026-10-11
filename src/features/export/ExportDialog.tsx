import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import clsx from "clsx";
import { useEffect, useState } from "react";
import { Button, Checkbox, Field, Input, Modal, Select, Spinner } from "../../components/ui";
import { api, errorText, onExportProgress, type Dialect, type ExportFormat, type ExportOptions } from "../../lib/ipc";
import { uid } from "../../store/app";

const FORMATS: { id: ExportFormat; label: string; ext: string; desc: string }[] = [
  { id: "csv", label: "CSV", ext: "csv", desc: "Comma-separated values" },
  { id: "tsv", label: "TSV", ext: "tsv", desc: "Tab-separated values" },
  { id: "xlsx", label: "Excel", ext: "xlsx", desc: "Excel workbook (.xlsx)" },
  { id: "json", label: "JSON", ext: "json", desc: "Array of objects" },
  { id: "jsonl", label: "JSON Lines", ext: "jsonl", desc: "One object per line" },
  { id: "sql", label: "SQL INSERT", ext: "sql", desc: "INSERT statements" },
  { id: "markdown", label: "Markdown", ext: "md", desc: "GitHub-flavored table" },
  { id: "html", label: "HTML", ext: "html", desc: "Standalone HTML table" },
  { id: "xml", label: "XML", ext: "xml", desc: "<rows><row>…" },
];

interface Props {
  resultId: string;
  rowCount: number;
  truncated: boolean;
  sessionId: string;
  sql: string;
  dialect: Dialect;
  defaultTable?: string;
  onClose: () => void;
}

export function ExportDialog({ resultId, rowCount, truncated, sessionId, sql, dialect, defaultTable, onClose }: Props) {
  const [o, setO] = useState<ExportOptions>({
    format: "csv",
    delimiter: ",",
    quoteAll: false,
    header: true,
    nullText: "",
    tableName: defaultTable || "exported_table",
    batchSize: 1,
    dialect,
    pretty: true,
    bom: false,
    maxRows: 0,
  });
  const [scope, setScope] = useState<"all" | "fetched">(truncated ? "all" : "fetched");
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [jobId] = useState(() => uid("x"));

  useEffect(() => {
    const un = onExportProgress((p) => p.jobId === jobId && setProgress(p.rows));
    return () => {
      un.then((f) => f());
    };
  }, [jobId]);

  const set = <K extends keyof ExportOptions>(k: K, v: ExportOptions[K]) => setO((p) => ({ ...p, [k]: v }));
  const fmt = FORMATS.find((f) => f.id === o.format)!;

  const toFile = async () => {
    const path = await saveDialog({
      defaultPath: `${(defaultTable || "result").replace(/[^\w.-]+/g, "_")}.${fmt.ext}`,
      filters: [{ name: fmt.label, extensions: [fmt.ext] }],
    });
    if (!path) return;
    setBusy(true);
    setMsg(null);
    setProgress(0);
    try {
      const n =
        scope === "all"
          ? await api.exportQuery(sessionId, sql, o, path, jobId)
          : await api.exportResult(resultId, o, path);
      setMsg({ ok: true, text: `Exported ${n.toLocaleString()} rows to ${path}` });
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  };

  const toClipboard = async () => {
    setBusy(true);
    setMsg(null);
    try {
      const text = await api.exportToString(resultId, o);
      await navigator.clipboard.writeText(text);
      setMsg({ ok: true, text: `Copied ${rowCount.toLocaleString()} rows to the clipboard.` });
    } catch (e) {
      setMsg({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Export Data"
      onClose={busy ? () => {} : onClose}
      width={640}
      footer={
        <>
          {busy && (
            <span className="mr-auto inline-flex items-center gap-2 text-xs text-muted">
              <Spinner size={12} /> {progress > 0 ? `${progress.toLocaleString()} rows…` : "Exporting…"}
            </span>
          )}
          <Button onClick={toClipboard} disabled={busy || o.format === "xlsx" || scope === "all"} title={scope === "all" ? "Clipboard export uses fetched rows only" : ""}>
            Copy to clipboard
          </Button>
          <Button variant="primary" onClick={toFile} disabled={busy}>
            Export to file…
          </Button>
          <Button onClick={onClose} disabled={busy}>
            Close
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="grid grid-cols-3 gap-1.5">
          {FORMATS.map((f) => (
            <button
              key={f.id}
              onClick={() => set("format", f.id)}
              className={clsx(
                "rounded-md border px-3 py-2 text-left",
                o.format === f.id ? "border-accent bg-accent-soft" : "border-line hover:bg-hover",
              )}
            >
              <div className="text-[13px] font-semibold">{f.label}</div>
              <div className="text-[11px] text-muted">{f.desc}</div>
            </button>
          ))}
        </div>

        <Field label="Rows">
          <div className="flex gap-4 text-[13px]">
            <label className="inline-flex items-center gap-1.5">
              <input type="radio" className="accent-[var(--accent)]" checked={scope === "all"} onChange={() => setScope("all")} />
              All rows (re-run query, streamed to file)
            </label>
            <label className="inline-flex items-center gap-1.5">
              <input type="radio" className="accent-[var(--accent)]" checked={scope === "fetched"} onChange={() => setScope("fetched")} />
              Fetched rows only ({rowCount.toLocaleString()})
            </label>
          </div>
        </Field>

        <div className="grid grid-cols-2 gap-3">
          {o.format === "csv" && (
            <Field label="Delimiter">
              <Select value={o.delimiter} onChange={(e) => set("delimiter", e.target.value)}>
                <option value=",">Comma ( , )</option>
                <option value=";">Semicolon ( ; )</option>
                <option value="|">Pipe ( | )</option>
              </Select>
            </Field>
          )}
          {["csv", "tsv", "markdown", "html"].includes(o.format) && (
            <Field label="NULL as">
              <Input value={o.nullText} onChange={(e) => set("nullText", e.target.value)} placeholder="(empty)" />
            </Field>
          )}
          {o.format === "sql" && (
            <>
              <Field label="Table name">
                <Input value={o.tableName} onChange={(e) => set("tableName", e.target.value)} />
              </Field>
              <Field label="Rows per INSERT">
                <Input type="number" min={1} value={o.batchSize} onChange={(e) => set("batchSize", Math.max(1, Number(e.target.value) || 1))} />
              </Field>
              <Field label="SQL dialect">
                <Select value={o.dialect} onChange={(e) => set("dialect", e.target.value)}>
                  {["postgres", "mysql", "oracle", "mssql", "sqlite", "generic"].map((d) => (
                    <option key={d}>{d}</option>
                  ))}
                </Select>
              </Field>
            </>
          )}
          {scope === "all" && (
            <Field label="Max rows (0 = unlimited)">
              <Input type="number" min={0} value={o.maxRows} onChange={(e) => set("maxRows", Math.max(0, Number(e.target.value) || 0))} />
            </Field>
          )}
        </div>

        <div className="flex flex-wrap gap-x-5 gap-y-2">
          {["csv", "tsv", "xlsx", "markdown", "html"].includes(o.format) && (
            <Checkbox label="Header row" checked={o.header} onChange={(v) => set("header", v)} />
          )}
          {["csv", "tsv"].includes(o.format) && (
            <>
              <Checkbox label="Quote all values" checked={o.quoteAll} onChange={(v) => set("quoteAll", v)} />
              <Checkbox label="UTF-8 BOM (for Excel)" checked={o.bom} onChange={(v) => set("bom", v)} />
            </>
          )}
          {o.format === "json" && <Checkbox label="Pretty print" checked={o.pretty} onChange={(v) => set("pretty", v)} />}
        </div>

        {msg && (
          <div className={clsx("rounded-md px-3 py-2 text-xs select-text", msg.ok ? "bg-ok/10 text-ok" : "bg-danger/10 text-danger")}>
            {msg.text}
          </div>
        )}
      </div>
    </Modal>
  );
}
