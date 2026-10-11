//! Key-based diff of two result sets (possibly from different databases).

use crate::model::{Cell, ColumnMeta};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    /// "only_a" | "only_b" | "changed"
    pub status: &'static str,
    pub a: Option<Vec<Cell>>,
    pub b: Option<Vec<Cell>>,
    /// Indexes (into `columns`) that differ, for "changed".
    pub changed: Vec<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    pub columns: Vec<String>,
    pub key_columns: Vec<String>,
    pub same: u64,
    pub changed: u64,
    pub only_a: u64,
    pub only_b: u64,
    pub duplicate_keys: u64,
    pub rows: Vec<DiffRow>,
    pub truncated: bool,
}

const MAX_DIFFS: usize = 5000;

fn norm(c: &Cell) -> String {
    match c {
        Cell::Null => "\u{0}NULL".into(),
        Cell::Bool(b) => (if *b { "1" } else { "0" }).into(),
        Cell::Int(i) => i.to_string(),
        Cell::Float(_) | Cell::Dec(_) => {
            let t = c.to_text();
            match t.parse::<f64>() {
                Ok(f) if f.fract() == 0.0 && f.abs() < 1e15 => (f as i64).to_string(),
                Ok(f) => format!("{f}"),
                Err(_) => t,
            }
        }
        Cell::Text(s) => s.trim_end().to_string(),
        other => other.to_text(),
    }
}

pub fn compare(
    a_cols: &[ColumnMeta],
    a_rows: &[Vec<Cell>],
    b_cols: &[ColumnMeta],
    b_rows: &[Vec<Cell>],
    keys: &[String],
) -> Result<CompareResult> {
    // Columns present in both, matched case-insensitively, in A's order.
    let pairs: Vec<(usize, usize, String)> = a_cols
        .iter()
        .enumerate()
        .filter_map(|(i, c)| b_cols.iter().position(|b| b.name.eq_ignore_ascii_case(&c.name)).map(|j| (i, j, c.name.clone())))
        .collect();
    if pairs.is_empty() {
        bail!("The two results have no column names in common");
    }
    let key_idx: Vec<usize> = if keys.is_empty() {
        (0..pairs.len()).collect()
    } else {
        keys.iter()
            .map(|k| pairs.iter().position(|p| p.2.eq_ignore_ascii_case(k)).ok_or_else(|| anyhow::anyhow!("key column '{k}' not in both results")))
            .collect::<Result<_>>()?
    };
    let key_of = |row: &[Cell], side_b: bool| -> String {
        key_idx
            .iter()
            .map(|&k| norm(&row[if side_b { pairs[k].1 } else { pairs[k].0 }]))
            .collect::<Vec<_>>()
            .join("\u{1}")
    };
    let project = |row: &[Cell], side_b: bool| -> Vec<Cell> {
        pairs.iter().map(|p| row[if side_b { p.1 } else { p.0 }].clone()).collect()
    };

    let mut b_map: HashMap<String, usize> = HashMap::with_capacity(b_rows.len());
    let mut dup = 0u64;
    for (i, r) in b_rows.iter().enumerate() {
        if b_map.insert(key_of(r, true), i).is_some() {
            dup += 1;
        }
    }
    let mut res = CompareResult {
        columns: pairs.iter().map(|p| p.2.clone()).collect(),
        key_columns: key_idx.iter().map(|&k| pairs[k].2.clone()).collect(),
        same: 0,
        changed: 0,
        only_a: 0,
        only_b: 0,
        duplicate_keys: dup,
        rows: vec![],
        truncated: false,
    };
    let mut seen_b = vec![false; b_rows.len()];
    let mut seen_a: HashMap<String, ()> = HashMap::new();
    let push = |res: &mut CompareResult, d: DiffRow| {
        if res.rows.len() < MAX_DIFFS {
            res.rows.push(d);
        } else {
            res.truncated = true;
        }
    };
    for r in a_rows {
        let k = key_of(r, false);
        if seen_a.insert(k.clone(), ()).is_some() {
            res.duplicate_keys += 1;
        }
        match b_map.get(&k) {
            Some(&j) => {
                seen_b[j] = true;
                let (pa, pb) = (project(r, false), project(&b_rows[j], true));
                let changed: Vec<usize> = (0..pa.len()).filter(|&i| norm(&pa[i]) != norm(&pb[i])).collect();
                if changed.is_empty() {
                    res.same += 1;
                } else {
                    res.changed += 1;
                    push(&mut res, DiffRow { status: "changed", a: Some(pa), b: Some(pb), changed });
                }
            }
            None => {
                res.only_a += 1;
                push(&mut res, DiffRow { status: "only_a", a: Some(project(r, false)), b: None, changed: vec![] });
            }
        }
    }
    for (j, r) in b_rows.iter().enumerate() {
        if !seen_b[j] {
            res.only_b += 1;
            push(&mut res, DiffRow { status: "only_b", a: None, b: Some(project(r, true)), changed: vec![] });
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_by_key() {
        let ca = vec![ColumnMeta::new("id", "", true), ColumnMeta::new("name", "", false), ColumnMeta::new("amt", "", true)];
        let cb = vec![ColumnMeta::new("NAME", "", false), ColumnMeta::new("ID", "", true), ColumnMeta::new("amt", "", true)];
        let a = vec![
            vec![Cell::Int(1), Cell::Text("a".into()), Cell::Dec("1.50".into())],
            vec![Cell::Int(2), Cell::Text("b".into()), Cell::Int(2)],
            vec![Cell::Int(3), Cell::Text("c".into()), Cell::Null],
        ];
        let b = vec![
            vec![Cell::Text("a".into()), Cell::Dec("1".into()), Cell::Float(1.5)],
            vec![Cell::Text("B".into()), Cell::Int(2), Cell::Dec("2.0".into())],
            vec![Cell::Text("d".into()), Cell::Int(4), Cell::Null],
        ];
        let r = compare(&ca, &a, &cb, &b, &["id".into()]).unwrap();
        assert_eq!((r.same, r.changed, r.only_a, r.only_b), (1, 1, 1, 1));
        let ch = r.rows.iter().find(|d| d.status == "changed").unwrap();
        assert_eq!(ch.changed, vec![1]);
    }
}
