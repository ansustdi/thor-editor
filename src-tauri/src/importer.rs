//! CSV / TSV / XLSX import into a SQL table via batched INSERTs.

use crate::drivers::{Dialect, Session};
use crate::export::sql_literal;
use crate::model::Cell;
use anyhow::{bail, Context, Result};
use calamine::{open_workbook_auto, Data, Reader};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceOptions {
    /// csv | tsv | xlsx (empty = by extension)
    pub format: String,
    pub delimiter: String,
    pub header: bool,
    pub sheet: String,
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self { format: String::new(), delimiter: ",".into(), header: true, sheet: String::new() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub sheets: Vec<String>,
    /// Suggested SQL type per column (from the preview rows).
    pub types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TargetOptions {
    /// Qualified, already-quoted table name.
    pub table: String,
    /// Target column (quoted) per source column; empty = skip.
    pub mapping: Vec<String>,
    pub create: bool,
    /// Column types for CREATE TABLE (same order as mapping).
    pub types: Vec<String>,
    pub truncate: bool,
    pub empty_as_null: bool,
    pub batch_size: usize,
}

impl Default for TargetOptions {
    fn default() -> Self {
        Self {
            table: String::new(),
            mapping: vec![],
            create: false,
            types: vec![],
            truncate: false,
            empty_as_null: true,
            batch_size: 200,
        }
    }
}

fn format_of(path: &Path, o: &SourceOptions) -> String {
    if !o.format.is_empty() {
        return o.format.clone();
    }
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("xlsx" | "xlsm" | "xls" | "xlsb" | "ods") => "xlsx".into(),
        Some("tsv" | "tab") => "tsv".into(),
        _ => "csv".into(),
    }
}

fn data_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(dt) => dt.as_datetime().map(|x| x.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| dt.to_string()),
        other => other.to_string(),
    }
}

/// Streams records (header row included when present) to `f`; returning false stops.
fn read_rows(path: &Path, o: &SourceOptions, mut f: impl FnMut(Vec<String>) -> Result<bool>) -> Result<Vec<String>> {
    match format_of(path, o).as_str() {
        "xlsx" => {
            let mut wb = open_workbook_auto(path).context("cannot open workbook")?;
            let sheets = wb.sheet_names().to_vec();
            let name = if o.sheet.is_empty() { sheets.first().cloned().unwrap_or_default() } else { o.sheet.clone() };
            let range = wb.worksheet_range(&name).with_context(|| format!("sheet '{name}'"))?;
            for row in range.rows() {
                if !f(row.iter().map(data_text).collect())? {
                    break;
                }
            }
            Ok(sheets)
        }
        fmt => {
            let delim = if fmt == "tsv" { b'\t' } else { o.delimiter.bytes().next().unwrap_or(b',') };
            let mut rdr = csv::ReaderBuilder::new()
                .delimiter(delim)
                .has_headers(false)
                .flexible(true)
                .from_path(path)
                .context("cannot open file")?;
            for rec in rdr.records() {
                let rec = rec?;
                let mut v: Vec<String> = rec.iter().map(String::from).collect();
                if let Some(first) = v.first_mut() {
                    *first = first.trim_start_matches('\u{feff}').to_string();
                }
                if !f(v)? {
                    break;
                }
            }
            Ok(vec![])
        }
    }
}

fn is_int(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty() && t.len() <= 18 && t.bytes().all(|b| b.is_ascii_digit()) && !(t.len() > 1 && t.starts_with('0'))
}

fn is_num(s: &str) -> bool {
    is_int(s) || (s.parse::<f64>().map(|f| f.is_finite()).unwrap_or(false) && !s.contains(['e', 'E', 'x']) && !s.starts_with('.') && !s.ends_with('.'))
}

pub fn type_names(d: Dialect) -> (&'static str, &'static str, &'static str) {
    // (integer, decimal, text)
    match d {
        Dialect::Postgres => ("bigint", "numeric", "text"),
        Dialect::Mysql => ("bigint", "decimal(38,10)", "text"),
        Dialect::Sqlite => ("INTEGER", "REAL", "TEXT"),
        Dialect::Mssql => ("bigint", "decimal(38,10)", "nvarchar(max)"),
        Dialect::Oracle => ("NUMBER(19)", "NUMBER", "VARCHAR2(4000)"),
        Dialect::Clickhouse => ("Nullable(Int64)", "Nullable(Float64)", "Nullable(String)"),
        _ => ("", "", ""),
    }
}

pub fn preview(path: &Path, o: &SourceOptions, dialect: Option<Dialect>) -> Result<Preview> {
    let mut rows: Vec<Vec<String>> = vec![];
    let sheets = read_rows(path, o, |r| {
        rows.push(r);
        Ok(rows.len() < 501)
    })?;
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let columns: Vec<String> = if o.header && !rows.is_empty() {
        let h = rows.remove(0);
        (0..width).map(|i| h.get(i).cloned().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("column{}", i + 1))).collect()
    } else {
        (0..width).map(|i| format!("column{}", i + 1)).collect()
    };
    let (ti, td, tt) = type_names(dialect.unwrap_or(Dialect::Postgres));
    let types = (0..width)
        .map(|i| {
            let vals: Vec<&str> = rows.iter().filter_map(|r| r.get(i)).map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if vals.is_empty() {
                tt
            } else if vals.iter().all(|v| is_int(v)) {
                ti
            } else if vals.iter().all(|v| is_num(v)) {
                td
            } else {
                tt
            }
            .to_string()
        })
        .collect();
    rows.truncate(100);
    Ok(Preview { columns, rows, sheets, types })
}

fn cell_for(v: &str, empty_as_null: bool) -> Cell {
    if v.is_empty() && empty_as_null {
        Cell::Null
    } else if is_num(v.trim()) {
        Cell::Dec(v.trim().to_string())
    } else {
        Cell::Text(v.to_string())
    }
}

fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::Postgres => "postgres",
        Dialect::Mysql => "mysql",
        Dialect::Sqlite => "sqlite",
        Dialect::Mssql => "mssql",
        Dialect::Oracle => "oracle",
        _ => "generic",
    }
}

fn insert_sql(d: Dialect, table: &str, cols: &str, rows: &[String]) -> String {
    match d {
        Dialect::Oracle if rows.len() > 1 => {
            let into: Vec<String> = rows.iter().map(|r| format!("INTO {table} ({cols}) VALUES {r}")).collect();
            format!("INSERT ALL\n{}\nSELECT 1 FROM dual", into.join("\n"))
        }
        _ => format!("INSERT INTO {table} ({cols}) VALUES {}", rows.join(",\n")),
    }
}

pub async fn run(
    s: &dyn Session,
    path: &Path,
    src: &SourceOptions,
    t: &TargetOptions,
    mut progress: impl FnMut(u64) + Send,
) -> Result<u64> {
    let d = s.dialect();
    if matches!(d, Dialect::Redis | Dialect::Mongo | Dialect::Neo4j | Dialect::Janus | Dialect::Elastic) {
        bail!("File import supports SQL databases only");
    }
    let targets: Vec<(usize, String)> =
        t.mapping.iter().enumerate().filter(|(_, c)| !c.trim().is_empty()).map(|(i, c)| (i, c.clone())).collect();
    if targets.is_empty() {
        bail!("Map at least one column");
    }
    if t.create {
        let defs: Vec<String> = targets
            .iter()
            .map(|(i, c)| format!("{c} {}", t.types.get(*i).filter(|x| !x.is_empty()).cloned().unwrap_or_else(|| type_names(d).2.into())))
            .collect();
        let engine = if d == Dialect::Clickhouse { " ENGINE = MergeTree ORDER BY tuple()" } else { "" };
        s.exec(&format!("CREATE TABLE {} ({}){engine}", t.table, defs.join(", "))).await.context("CREATE TABLE failed")?;
    }
    if t.truncate {
        let sql = if d == Dialect::Sqlite { format!("DELETE FROM {}", t.table) } else { format!("TRUNCATE TABLE {}", t.table) };
        s.exec(&sql).await.context("truncate failed")?;
    }

    // Read on a blocking thread, feed batches through a channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Vec<String>>>(4);
    let path_owned = path.to_path_buf();
    let src_owned = src.clone();
    let batch = t.batch_size.clamp(1, if d == Dialect::Mssql { 1000 } else { 5000 });
    let reader = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut first = true;
        let mut buf = Vec::with_capacity(batch);
        read_rows(&path_owned, &src_owned, |r| {
            if first && src_owned.header {
                first = false;
                return Ok(true);
            }
            first = false;
            if r.iter().all(|c| c.is_empty()) {
                return Ok(true);
            }
            buf.push(r);
            if buf.len() >= batch {
                if tx.blocking_send(std::mem::take(&mut buf)).is_err() {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        if !buf.is_empty() {
            let _ = tx.blocking_send(buf);
        }
        Ok(())
    });

    let cols = targets.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>().join(", ");
    let lit_d = dialect_name(d);
    let mut total = 0u64;
    while let Some(rows) = rx.recv().await {
        let values: Vec<String> = rows
            .iter()
            .map(|r| {
                let vs: Vec<String> = targets
                    .iter()
                    .map(|(i, _)| sql_literal(lit_d, &cell_for(r.get(*i).map(String::as_str).unwrap_or(""), t.empty_as_null)))
                    .collect();
                format!("({})", vs.join(", "))
            })
            .collect();
        let sql = insert_sql(d, &t.table, &cols, &values);
        s.exec(&sql).await.with_context(|| format!("insert failed after {total} rows"))?;
        total += rows.len() as u64;
        progress(total);
    }
    reader.await??;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_detection() {
        assert!(is_int("42") && is_int("-7") && !is_int("007") && !is_int("1.5"));
        assert!(is_num("1.5") && is_num("-0.25") && !is_num("1e5") && !is_num("abc") && !is_num(""));
    }

    #[test]
    fn preview_csv() {
        let dir = std::env::temp_dir().join(format!("imp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.csv");
        std::fs::write(&f, "\u{feff}id,name,price\n1,Ann,1.5\n2,\"B, b\",3\n").unwrap();
        let p = preview(&f, &SourceOptions::default(), Some(Dialect::Postgres)).unwrap();
        assert_eq!(p.columns, vec!["id", "name", "price"]);
        assert_eq!(p.rows[1][1], "B, b");
        assert_eq!(p.types, vec!["bigint", "text", "numeric"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn oracle_insert_all() {
        let s = insert_sql(Dialect::Oracle, "T", "A", &["(1)".into(), "(2)".into()]);
        assert!(s.starts_with("INSERT ALL\nINTO T (A) VALUES (1)\nINTO T (A) VALUES (2)\nSELECT 1 FROM dual"));
    }
}
