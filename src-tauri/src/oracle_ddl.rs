//! Fast Oracle DDL straight from the data dictionary (what Toad does by default).
//! DBMS_METADATA.GET_DDL is exact but often takes seconds per object; this takes milliseconds.

use crate::drivers::{lit, CollectSink, Session};
use crate::model::Cell;
use anyhow::Result;

async fn rows(s: &dyn Session, sql: &str) -> Result<Vec<Vec<Cell>>> {
    let mut sink = CollectSink::new(0);
    s.run(sql, &mut sink).await?;
    Ok(sink.rows)
}

fn t(c: Option<&Cell>) -> String {
    c.map(Cell::to_text).unwrap_or_default()
}

fn n(c: Option<&Cell>) -> Option<i64> {
    match c? {
        Cell::Null => None,
        v => v.to_text().parse().ok(),
    }
}

/// Plain upper-case identifiers stay bare; anything else is quoted.
pub fn ident(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'));
    if plain {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Column type as written in DDL (VARCHAR2(30 BYTE), NUMBER(10,2), TIMESTAMP(6) …).
pub fn column_type(dt: &str, len: Option<i64>, prec: Option<i64>, scale: Option<i64>, char_len: Option<i64>, char_used: &str) -> String {
    let unit = if char_used == "C" { "CHAR" } else { "BYTE" };
    match dt {
        "VARCHAR2" | "CHAR" => format!("{dt}({} {unit})", char_len.or(len).unwrap_or(1)),
        "NVARCHAR2" | "NCHAR" => format!("{dt}({})", char_len.or(len).unwrap_or(1)),
        "RAW" => format!("RAW({})", len.unwrap_or(1)),
        "NUMBER" => match (prec, scale) {
            (None, None) => "NUMBER".into(),
            (None, Some(0)) => "INTEGER".into(),
            (None, Some(s)) => format!("NUMBER(*,{s})"),
            (Some(p), Some(0)) | (Some(p), None) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
        },
        "FLOAT" => prec.map(|p| format!("FLOAT({p})")).unwrap_or_else(|| "FLOAT".into()),
        other => other.to_string(),
    }
}

pub async fn table(s: &dyn Session, schema: &str, name: &str) -> Result<String> {
    let (qs, qn) = (lit(schema), lit(name));
    let full = format!("{}.{}", ident(schema), ident(name));

    let cols = rows(
        s,
        &format!(
            "select column_name, data_type, data_length, data_precision, data_scale, char_length, char_used, nullable, data_default
             from all_tab_columns where owner = {qs} and table_name = {qn} order by column_id"
        ),
    )
    .await?;
    let info = rows(
        s,
        &format!("select tablespace_name, partitioned, temporary, duration, iot_type, logging from all_tables where owner = {qs} and table_name = {qn}"),
    )
    .await?;
    // Plain single-view queries, joined in Rust: dictionary joins / LISTAGG subqueries over
    // ALL_* views cost seconds on large databases, these stay in the low milliseconds.
    let cons = rows(
        s,
        &format!(
            "select constraint_name, constraint_type, search_condition_vc, delete_rule, generated, status, r_owner, r_constraint_name
             from all_constraints where owner = {qs} and table_name = {qn} and constraint_type in ('P', 'U', 'R', 'C')"
        ),
    )
    .await
    .unwrap_or_default();
    let cons_cols = rows(
        s,
        &format!("select constraint_name, column_name from all_cons_columns where owner = {qs} and table_name = {qn} order by constraint_name, position"),
    )
    .await
    .unwrap_or_default();
    // Referenced (parent) keys of foreign keys.
    let refs: Vec<(String, String)> = cons
        .iter()
        .filter(|r| t(r.get(1)) == "R")
        .map(|r| (t(r.get(6)), t(r.get(7))))
        .collect();
    let mut ref_tables: std::collections::HashMap<(String, String), String> = Default::default();
    let mut ref_cols: std::collections::HashMap<(String, String), Vec<String>> = Default::default();
    if !refs.is_empty() {
        let names = refs.iter().map(|(_, c)| lit(c)).collect::<Vec<_>>().join(", ");
        let owners = refs.iter().map(|(o, _)| lit(o)).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", ");
        for r in rows(s, &format!("select owner, constraint_name, table_name from all_constraints where owner in ({owners}) and constraint_name in ({names})"))
            .await
            .unwrap_or_default()
        {
            ref_tables.insert((t(r.first()), t(r.get(1))), t(r.get(2)));
        }
        for r in rows(
            s,
            &format!("select owner, constraint_name, column_name from all_cons_columns where owner in ({owners}) and constraint_name in ({names}) order by position"),
        )
        .await
        .unwrap_or_default()
        {
            ref_cols.entry((t(r.first()), t(r.get(1)))).or_default().push(ident(&t(r.get(2))));
        }
    }
    let mut cols_of: std::collections::HashMap<String, Vec<String>> = Default::default();
    for r in &cons_cols {
        cols_of.entry(t(r.first())).or_default().push(ident(&t(r.get(1))));
    }
    let backing: std::collections::HashSet<String> = rows(
        s,
        &format!("select index_name from all_constraints where owner = {qs} and table_name = {qn} and constraint_type in ('P', 'U') and index_name is not null"),
    )
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| t(r.first()))
    .collect();
    let indexes = rows(
        s,
        &format!("select owner, index_name, uniqueness, index_type from all_indexes where table_owner = {qs} and table_name = {qn} order by index_name"),
    )
    .await
    .unwrap_or_default();
    let ind_cols = rows(
        s,
        &format!("select index_name, column_name, descend, column_position from all_ind_columns where table_owner = {qs} and table_name = {qn} order by index_name, column_position"),
    )
    .await
    .unwrap_or_default();
    // DESC / function-based index columns are hidden SYS_NC…$ columns; their text is here.
    let ind_expr = rows(
        s,
        &format!("select index_name, column_position, column_expression from all_ind_expressions where table_owner = {qs} and table_name = {qn}"),
    )
    .await
    .unwrap_or_default();
    let expr_of: std::collections::HashMap<(String, i64), String> =
        ind_expr.iter().map(|r| ((t(r.first()), n(r.get(1)).unwrap_or(0)), t(r.get(2)))).collect();
    let mut idx_cols: std::collections::HashMap<String, Vec<String>> = Default::default();
    for r in &ind_cols {
        let (iname, col, desc, pos) = (t(r.first()), t(r.get(1)), t(r.get(2)), n(r.get(3)).unwrap_or(0));
        let base = match expr_of.get(&(iname.clone(), pos)) {
            Some(e) => {
                let e = e.trim();
                // DESC columns store the column name quoted, e.g. "CREATED"
                if e.starts_with('"') && e.ends_with('"') && e.len() > 2 { ident(&e[1..e.len() - 1]) } else { e.to_string() }
            }
            None => ident(&col),
        };
        idx_cols.entry(iname).or_default().push(if desc == "DESC" { format!("{base} DESC") } else { base });
    }
    let idx: Vec<Vec<Cell>> = indexes
        .into_iter()
        .filter(|r| !backing.contains(&t(r.get(1))))
        .map(|mut r| {
            let cols = idx_cols.get(&t(r.get(1))).cloned().unwrap_or_default().join(", ");
            r.push(Cell::Text(cols));
            r
        })
        .collect();
    let tab_comment = rows(s, &format!("select comments from all_tab_comments where owner = {qs} and table_name = {qn} and comments is not null")).await?;
    let col_comments = rows(
        s,
        &format!("select column_name, comments from all_col_comments where owner = {qs} and table_name = {qn} and comments is not null order by column_name"),
    )
    .await?;

    let temporary = info.first().map(|r| t(r.get(2)) == "Y").unwrap_or(false);
    let mut out = String::new();
    out.push_str(&format!("CREATE {}TABLE {full}\n(\n", if temporary { "GLOBAL TEMPORARY " } else { "" }));

    let width = cols.iter().map(|r| ident(&t(r.first())).len()).max().unwrap_or(0);
    let defs: Vec<(String, String, String, bool)> = cols
        .iter()
        .map(|r| {
            let ty = column_type(&t(r.get(1)), n(r.get(2)), n(r.get(3)), n(r.get(4)), n(r.get(5)), &t(r.get(6)));
            let default = t(r.get(8)).trim().to_string();
            (ident(&t(r.first())), ty, default, t(r.get(7)) == "N")
        })
        .collect();
    let tw = defs.iter().map(|d| d.1.len()).max().unwrap_or(0);
    let lines: Vec<String> = defs
        .iter()
        .map(|(c, ty, def, not_null)| {
            let mut l = format!("  {c:<width$}  {ty:<tw$}");
            if !def.is_empty() {
                l.push_str(&format!(" DEFAULT {def}"));
            }
            if *not_null {
                l.push_str(" NOT NULL");
            }
            l.trim_end().to_string()
        })
        .collect();
    out.push_str(&lines.join(",\n"));
    out.push_str("\n)");
    if let Some(r) = info.first() {
        if temporary {
            out.push_str(if t(r.get(3)) == "SYS$SESSION" { "\nON COMMIT PRESERVE ROWS" } else { "\nON COMMIT DELETE ROWS" });
        } else if !t(r.first()).is_empty() {
            out.push_str(&format!("\nTABLESPACE {}", ident(&t(r.first()))));
        }
        if t(r.get(5)) == "NO" {
            out.push_str("\nNOLOGGING");
        }
    }
    out.push_str(";\n");
    if info.first().map(|r| t(r.get(1)) == "YES").unwrap_or(false) {
        out.push_str("-- Partitioned table: use \"Full DDL\" for the partitioning clause.\n");
    }

    if let Some(r) = tab_comment.first() {
        out.push_str(&format!("\nCOMMENT ON TABLE {full} IS {};\n", q(&t(r.first()))));
    }
    if !col_comments.is_empty() {
        out.push('\n');
        for r in &col_comments {
            out.push_str(&format!("COMMENT ON COLUMN {full}.{} IS {};\n", ident(&t(r.first())), q(&t(r.get(1)))));
        }
    }

    for r in &idx {
        let unique = if t(r.get(2)) == "UNIQUE" { "UNIQUE " } else { "" };
        let bitmap = if t(r.get(3)) == "BITMAP" { "BITMAP " } else { "" };
        out.push_str(&format!(
            "\nCREATE {unique}{bitmap}INDEX {}.{} ON {full}\n({});\n",
            ident(&t(r.first())),
            ident(&t(r.get(1))),
            t(r.get(4))
        ));
    }

    let not_null_check = |cond: &str| {
        let c = cond.trim();
        c.ends_with("IS NOT NULL") && !c.contains(" AND ") && !c.contains(" OR ")
    };
    let mut cons = cons;
    cons.sort_by_key(|r| (match t(r.get(1)).as_str() { "P" => 1, "U" => 2, "R" => 3, _ => 4 }, t(r.first())));
    let mut alters = vec![];
    for r in &cons {
        let (cname, ctype, cond, rule, generated, status) = (t(r.first()), t(r.get(1)), t(r.get(2)), t(r.get(3)), t(r.get(4)), t(r.get(5)));
        let cols = cols_of.get(&cname).cloned().unwrap_or_default().join(", ");
        if ctype == "C" && generated == "GENERATED NAME" && not_null_check(&cond) {
            continue; // already written as NOT NULL on the column
        }
        let named = if generated == "GENERATED NAME" { String::new() } else { format!("CONSTRAINT {} ", ident(&cname)) };
        let body = match ctype.as_str() {
            "P" => format!("PRIMARY KEY ({cols})"),
            "U" => format!("UNIQUE ({cols})"),
            "C" => format!("CHECK ({cond})"),
            _ => {
                let key = (t(r.get(6)), t(r.get(7)));
                let mut b = format!(
                    "FOREIGN KEY ({cols})
  REFERENCES {}.{} ({})",
                    ident(&key.0),
                    ident(ref_tables.get(&key).map(String::as_str).unwrap_or("?")),
                    ref_cols.get(&key).cloned().unwrap_or_default().join(", ")
                );
                if rule == "CASCADE" {
                    b.push_str("
  ON DELETE CASCADE");
                } else if rule == "SET NULL" {
                    b.push_str("
  ON DELETE SET NULL");
                }
                b
            }
        };
        let state = if status == "DISABLED" { "\n  DISABLE" } else { "\n  ENABLE VALIDATE" };
        alters.push(format!("ALTER TABLE {full} ADD (\n  {named}{body}{state});\n"));
    }
    for a in alters {
        out.push('\n');
        out.push_str(&a);
    }
    Ok(out.trim_end().to_string())
}

pub async fn view(s: &dyn Session, schema: &str, name: &str) -> Result<String> {
    let (qs, qn) = (lit(schema), lit(name));
    let text = rows(s, &format!("select text from all_views where owner = {qs} and view_name = {qn}")).await?;
    let cols = rows(s, &format!("select column_name from all_tab_columns where owner = {qs} and table_name = {qn} order by column_id")).await?;
    let list = cols.iter().map(|r| ident(&t(r.first()))).collect::<Vec<_>>().join(",\n   ");
    Ok(format!(
        "CREATE OR REPLACE FORCE VIEW {}.{}\n(\n   {list}\n)\nAS\n{};",
        ident(schema),
        ident(name),
        text.first().map(|r| t(r.first())).unwrap_or_default().trim_end().trim_end_matches(';')
    ))
}

pub async fn sequence(s: &dyn Session, schema: &str, name: &str) -> Result<String> {
    let r = rows(
        s,
        &format!(
            "select min_value, max_value, increment_by, cycle_flag, order_flag, cache_size, last_number
             from all_sequences where sequence_owner = {} and sequence_name = {}",
            lit(schema),
            lit(name)
        ),
    )
    .await?;
    let Some(r) = r.first() else { return Ok(String::new()) };
    let cache = n(r.get(5)).unwrap_or(0);
    Ok(format!(
        "CREATE SEQUENCE {}.{}\n  START WITH {}\n  MAXVALUE {}\n  MINVALUE {}\n  {}\n  {}\n  {}\n  INCREMENT BY {};",
        ident(schema),
        ident(name),
        t(r.get(6)),
        t(r.get(1)),
        t(r.first()),
        if t(r.get(3)) == "Y" { "CYCLE" } else { "NOCYCLE" },
        if cache > 0 { format!("CACHE {cache}") } else { "NOCACHE".into() },
        if t(r.get(4)) == "Y" { "ORDER" } else { "NOORDER" },
        t(r.get(2))
    ))
}

pub async fn synonym(s: &dyn Session, schema: &str, name: &str) -> Result<String> {
    let r = rows(
        s,
        &format!("select table_owner, table_name, db_link from all_synonyms where owner = {} and synonym_name = {}", lit(schema), lit(name)),
    )
    .await?;
    let Some(r) = r.first() else { return Ok(String::new()) };
    let link = t(r.get(2));
    let public = schema == "PUBLIC";
    Ok(format!(
        "CREATE OR REPLACE {}SYNONYM {}{} FOR {}.{}{};",
        if public { "PUBLIC " } else { "" },
        if public { String::new() } else { format!("{}.", ident(schema)) },
        ident(name),
        ident(&t(r.first())),
        ident(&t(r.get(1))),
        if link.is_empty() { String::new() } else { format!("@{link}") }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_idents() {
        assert_eq!(column_type("VARCHAR2", Some(30), None, None, Some(30), "B"), "VARCHAR2(30 BYTE)");
        assert_eq!(column_type("VARCHAR2", Some(120), None, None, Some(30), "C"), "VARCHAR2(30 CHAR)");
        assert_eq!(column_type("NUMBER", Some(22), Some(10), Some(0), None, ""), "NUMBER(10)");
        assert_eq!(column_type("NUMBER", Some(22), Some(18), Some(2), None, ""), "NUMBER(18,2)");
        assert_eq!(column_type("NUMBER", Some(22), None, Some(0), None, ""), "INTEGER");
        assert_eq!(column_type("NUMBER", Some(22), None, None, None, ""), "NUMBER");
        assert_eq!(column_type("TIMESTAMP(6)", Some(11), None, Some(6), None, ""), "TIMESTAMP(6)");
        assert_eq!(ident("ADM_OPERATION"), "ADM_OPERATION");
        assert_eq!(ident("Mixed"), "\"Mixed\"");
    }
}
