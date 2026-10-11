//! Streaming result exporters. Every exporter is a `RowSink`, so it can consume either
//! an in-memory result or a live query re-executed straight to disk.

use crate::drivers::RowSink;
use crate::model::{Cell, ColumnMeta};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExportOptions {
    /// csv | tsv | json | jsonl | xlsx | sql | markdown | html | xml
    pub format: String,
    pub delimiter: String,
    pub quote_all: bool,
    pub header: bool,
    pub null_text: String,
    pub table_name: String,
    /// Rows per INSERT statement (1 = one statement per row).
    pub batch_size: usize,
    /// Dialect used for SQL literals: postgres | mysql | sqlite | mssql | oracle | generic
    pub dialect: String,
    pub pretty: bool,
    pub bom: bool,
    pub max_rows: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: "csv".into(),
            delimiter: ",".into(),
            quote_all: false,
            header: true,
            null_text: String::new(),
            table_name: "exported_table".into(),
            batch_size: 1,
            dialect: "generic".into(),
            pretty: true,
            bom: false,
            max_rows: 0,
        }
    }
}

/// `Write` target that can be read back for clipboard export.
#[derive(Clone, Default)]
pub struct SharedBuf(pub Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub enum Target {
    File(PathBuf),
    Buffer(SharedBuf),
}

type Progress = Box<dyn FnMut(u64) + Send>;

enum Backend {
    Text(Box<dyn Write + Send>),
    Csv(csv::Writer<Box<dyn Write + Send>>),
    Xlsx { wb: rust_xlsxwriter::Workbook, path: PathBuf, row: u32 },
}

pub struct Exporter {
    opts: ExportOptions,
    backend: Backend,
    cols: Vec<ColumnMeta>,
    count: u64,
    batch: Vec<String>,
    progress: Option<Progress>,
}

impl Exporter {
    pub fn new(opts: ExportOptions, target: Target, progress: Option<Progress>) -> Result<Self> {
        let open = |t: Target| -> Result<Box<dyn Write + Send>> {
            Ok(match t {
                Target::File(p) => Box::new(BufWriter::with_capacity(1 << 16, std::fs::File::create(p)?)),
                Target::Buffer(b) => Box::new(b),
            })
        };
        let backend = match opts.format.as_str() {
            "xlsx" => match target {
                Target::File(path) => Backend::Xlsx { wb: rust_xlsxwriter::Workbook::new(), path, row: 0 },
                Target::Buffer(_) => bail!("XLSX cannot be copied to the clipboard"),
            },
            "csv" | "tsv" => {
                let mut w = open(target)?;
                if opts.bom {
                    w.write_all(b"\xEF\xBB\xBF")?;
                }
                let delim = if opts.format == "tsv" { b'\t' } else { opts.delimiter.bytes().next().unwrap_or(b',') };
                let csvw = csv::WriterBuilder::new()
                    .delimiter(delim)
                    .quote_style(if opts.quote_all { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
                    .from_writer(w);
                Backend::Csv(csvw)
            }
            "json" | "jsonl" | "sql" | "markdown" | "html" | "xml" => Backend::Text(open(target)?),
            other => bail!("Unknown export format '{other}'"),
        };
        Ok(Self { opts, backend, cols: vec![], count: 0, batch: vec![], progress })
    }

    fn out(&mut self) -> &mut Box<dyn Write + Send> {
        match &mut self.backend {
            Backend::Text(w) => w,
            _ => unreachable!("text backend expected"),
        }
    }

    pub fn finish(mut self) -> Result<u64> {
        self.flush_batch()?;
        let fmt = self.opts.format.clone();
        let pretty = self.opts.pretty;
        match &mut self.backend {
            Backend::Csv(w) => w.flush()?,
            Backend::Xlsx { wb, path, .. } => {
                if self.count == 0 && self.cols.is_empty() {
                    wb.add_worksheet();
                }
                wb.save(&*path)?;
            }
            Backend::Text(w) => {
                match fmt.as_str() {
                    "json" => w.write_all(if pretty { b"\n]\n" } else { b"]" })?,
                    "html" => w.write_all(b"</tbody>\n</table>\n</body>\n</html>\n")?,
                    "xml" => w.write_all(b"</rows>\n")?,
                    _ => {}
                }
                w.flush()?;
            }
        }
        Ok(self.count)
    }

    fn fmt_null(&self, c: &Cell) -> String {
        if c.is_null() {
            self.opts.null_text.clone()
        } else {
            c.to_text()
        }
    }

    fn flush_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let cols = self.cols.iter().map(|c| quote_ident(&self.opts.dialect, &c.name)).collect::<Vec<_>>().join(", ");
        let table = self.opts.table_name.clone();
        let rows = std::mem::take(&mut self.batch);
        let stmt = if rows.len() == 1 {
            format!("INSERT INTO {table} ({cols}) VALUES {};\n", rows[0])
        } else {
            format!("INSERT INTO {table} ({cols}) VALUES\n  {};\n", rows.join(",\n  "))
        };
        self.out().write_all(stmt.as_bytes())?;
        Ok(())
    }
}

fn quote_ident(dialect: &str, s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit();
    if plain {
        return s.to_string();
    }
    match dialect {
        "mysql" => format!("`{}`", s.replace('`', "``")),
        "mssql" => format!("[{}]", s.replace(']', "]]")),
        _ => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

pub fn sql_literal(dialect: &str, c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".into(),
        Cell::Bool(b) => match dialect {
            "oracle" | "mssql" | "sqlite" => (if *b { "1" } else { "0" }).into(),
            _ => (if *b { "TRUE" } else { "FALSE" }).into(),
        },
        Cell::Int(_) | Cell::Float(_) | Cell::Dec(_) => c.to_text(),
        Cell::Text(s) | Cell::DateTime(s) => {
            let q = format!("'{}'", s.replace('\'', "''"));
            if dialect == "mssql" && !s.is_ascii() {
                format!("N{q}")
            } else {
                q
            }
        }
        Cell::Bytes(b) => {
            let h = hex::encode_upper(b);
            match dialect {
                "postgres" => format!("'\\x{h}'::bytea"),
                "mssql" => format!("0x{h}"),
                "oracle" => format!("HEXTORAW('{h}')"),
                _ => format!("X'{h}'"),
            }
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn md_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace('\r', "").replace('\n', "<br>")
}

impl RowSink for Exporter {
    fn columns(&mut self, cols: Vec<ColumnMeta>) -> Result<()> {
        self.cols = cols;
        let header = self.opts.header;
        let names: Vec<String> = self.cols.iter().map(|c| c.name.clone()).collect();
        let pretty = self.opts.pretty;
        match self.opts.format.as_str() {
            "csv" | "tsv" if header => {
                if let Backend::Csv(w) = &mut self.backend {
                    w.write_record(&names)?;
                }
            }
            "xlsx" => {
                if let Backend::Xlsx { wb, row, .. } = &mut self.backend {
                    let ws = wb.add_worksheet_with_constant_memory();
                    ws.set_name("Result")?;
                    if header {
                        let bold = rust_xlsxwriter::Format::new().set_bold();
                        for (i, n) in names.iter().enumerate() {
                            ws.set_column_width(i as u16, (n.len().clamp(8, 40) + 2) as f64)?;
                            ws.write_string_with_format(0, i as u16, n, &bold)?;
                        }
                        ws.set_freeze_panes(1, 0)?;
                        *row = 1;
                    }
                }
            }
            "json" => self.out().write_all(if pretty { b"[\n" } else { b"[" })?,
            "markdown" if header => {
                let head = format!(
                    "| {} |\n|{}|\n",
                    names.iter().map(|n| md_escape(n)).collect::<Vec<_>>().join(" | "),
                    self.cols.iter().map(|c| if c.numeric { " ---: " } else { " --- " }).collect::<Vec<_>>().join("|")
                );
                self.out().write_all(head.as_bytes())?;
            }
            "html" => {
                let mut s = String::from(
                    "<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\"><title>Export</title>\n<style>table{border-collapse:collapse;font:13px system-ui}td,th{border:1px solid #ccc;padding:4px 8px}th{background:#f3f4f6;text-align:left}td.n{text-align:right}</style></head>\n<body>\n<table>\n",
                );
                if header {
                    s.push_str("<thead><tr>");
                    for n in &names {
                        s.push_str(&format!("<th>{}</th>", html_escape(n)));
                    }
                    s.push_str("</tr></thead>\n");
                }
                s.push_str("<tbody>\n");
                self.out().write_all(s.as_bytes())?;
            }
            "xml" => self.out().write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rows>\n")?,
            _ => {}
        }
        Ok(())
    }

    fn row(&mut self, row: Vec<Cell>) -> Result<bool> {
        if self.opts.max_rows > 0 && self.count as usize >= self.opts.max_rows {
            return Ok(false);
        }
        let fmt = self.opts.format.clone();
        match fmt.as_str() {
            "csv" | "tsv" => {
                let rec: Vec<String> = row.iter().map(|c| self.fmt_null(c)).collect();
                if let Backend::Csv(w) = &mut self.backend {
                    w.write_record(&rec)?;
                }
            }
            "xlsx" => {
                if let Backend::Xlsx { wb, row: r, .. } = &mut self.backend {
                    if *r >= 1_048_575 {
                        bail!("XLSX row limit (1,048,576) reached; use CSV for larger exports");
                    }
                    let ws = wb.worksheet_from_index(0)?;
                    for (i, c) in row.iter().enumerate() {
                        let col = i as u16;
                        match c {
                            Cell::Null => {}
                            Cell::Bool(b) => {
                                ws.write_boolean(*r, col, *b)?;
                            }
                            Cell::Int(v) if v.abs() < 1_000_000_000_000_000 => {
                                ws.write_number(*r, col, *v as f64)?;
                            }
                            Cell::Float(v) if v.is_finite() => {
                                ws.write_number(*r, col, *v)?;
                            }
                            Cell::Dec(s) if s.len() <= 15 && s.parse::<f64>().is_ok() => {
                                ws.write_number(*r, col, s.parse::<f64>().unwrap())?;
                            }
                            other => {
                                let t = other.to_text();
                                let t = if t.len() > 32_767 { t[..32_767].to_string() } else { t };
                                ws.write_string(*r, col, t)?;
                            }
                        }
                    }
                    *r += 1;
                }
            }
            "json" | "jsonl" => {
                let mut obj = serde_json::Map::with_capacity(row.len());
                for (c, v) in self.cols.iter().zip(&row) {
                    obj.insert(c.name.clone(), serde_json::to_value(v)?);
                }
                let v = serde_json::Value::Object(obj);
                let pretty = self.opts.pretty && fmt == "json";
                let mut s = if pretty { serde_json::to_string_pretty(&v)? } else { serde_json::to_string(&v)? };
                if fmt == "json" {
                    if pretty {
                        s = s.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
                    }
                    if self.count > 0 {
                        s = format!("{}{s}", if pretty { ",\n" } else { "," });
                    }
                } else {
                    s.push('\n');
                }
                self.out().write_all(s.as_bytes())?;
            }
            "sql" => {
                let d = self.opts.dialect.clone();
                let vals = format!("({})", row.iter().map(|c| sql_literal(&d, c)).collect::<Vec<_>>().join(", "));
                self.batch.push(vals);
                if self.batch.len() >= self.opts.batch_size.max(1) {
                    self.flush_batch()?;
                }
            }
            "markdown" => {
                let line = format!(
                    "| {} |\n",
                    row.iter().map(|c| md_escape(&self.fmt_null(c))).collect::<Vec<_>>().join(" | ")
                );
                self.out().write_all(line.as_bytes())?;
            }
            "html" => {
                let mut s = String::from("<tr>");
                for (c, meta) in row.iter().zip(&self.cols) {
                    let cls = if meta.numeric { " class=\"n\"" } else { "" };
                    s.push_str(&format!("<td{cls}>{}</td>", html_escape(&self.fmt_null(c))));
                }
                s.push_str("</tr>\n");
                self.out().write_all(s.as_bytes())?;
            }
            "xml" => {
                let mut s = String::from("  <row>\n");
                for (c, meta) in row.iter().zip(&self.cols) {
                    if c.is_null() {
                        s.push_str(&format!("    <column name=\"{}\" null=\"true\"/>\n", html_escape(&meta.name)));
                    } else {
                        s.push_str(&format!(
                            "    <column name=\"{}\">{}</column>\n",
                            html_escape(&meta.name),
                            html_escape(&c.to_text())
                        ));
                    }
                }
                s.push_str("  </row>\n");
                self.out().write_all(s.as_bytes())?;
            }
            _ => {}
        }
        self.count += 1;
        if self.count % 1000 == 0 {
            if let Some(p) = self.progress.as_mut() {
                p(self.count);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(format: &str, extra: impl FnOnce(&mut ExportOptions)) -> String {
        let mut o = ExportOptions { format: format.into(), ..Default::default() };
        extra(&mut o);
        let buf = SharedBuf::default();
        let mut e = Exporter::new(o, Target::Buffer(buf.clone()), None).unwrap();
        e.columns(vec![ColumnMeta::new("id", "int", true), ColumnMeta::new("name", "text", false)]).unwrap();
        e.row(vec![Cell::Int(1), Cell::Text("O'Brien, \"J\"".into())]).unwrap();
        e.row(vec![Cell::Int(2), Cell::Null]).unwrap();
        e.finish().unwrap();
        let v = buf.0.lock().unwrap().clone();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn csv_quotes() {
        assert_eq!(run("csv", |_| {}), "id,name\n1,\"O'Brien, \"\"J\"\"\"\n2,\n");
    }

    #[test]
    fn json_array() {
        let v: serde_json::Value = serde_json::from_str(&run("json", |_| {})).unwrap();
        assert_eq!(v[0]["id"], 1);
        assert!(v[1]["name"].is_null());
    }

    #[test]
    fn sql_insert_batch() {
        let s = run("sql", |o| {
            o.table_name = "t".into();
            o.batch_size = 10;
        });
        assert_eq!(s, "INSERT INTO t (id, name) VALUES\n  (1, 'O''Brien, \"J\"'),\n  (2, NULL);\n");
    }

    #[test]
    fn markdown_table() {
        let s = run("markdown", |_| {});
        assert!(s.starts_with("| id | name |\n| ---: | --- |\n| 1 |"));
    }
}
