//! Driver integration tests against `docker compose up -d` (see docker-compose.yml).
//! Run with:  DBFORGE_IT=1 cargo test it_ -- --test-threads=1

use crate::drivers::{connect, CollectSink, Session};
use crate::meta;
use crate::model::{Cell, ConnectionProfile};
use std::sync::Arc;

const PW: &str = "Passw0rd!";

fn enabled() -> bool {
    std::env::var("DBFORGE_IT").is_ok()
}

fn profile(v: serde_json::Value) -> ConnectionProfile {
    let mut v = v;
    v["id"] = "it".into();
    serde_json::from_value(v).unwrap()
}

async fn rows(s: &Arc<dyn Session>, sql: &str) -> Vec<Vec<Cell>> {
    let mut sink = CollectSink::new(0);
    s.run(sql, &mut sink).await.unwrap_or_else(|e| panic!("{sql}: {e:#}"));
    sink.rows
}

/// Shared scenario: DDL, DML, typed select, truncation, manual transactions, metadata.
async fn scenario(s: Arc<dyn Session>, schema: &str, create: &str, insert: &str) {
    s.set_autocommit(true).await.unwrap();
    let _ = s.exec("DROP TABLE it_emp").await;
    s.exec(create).await.unwrap();
    let mut sink = CollectSink::new(0);
    let out = s.run(insert, &mut sink).await.unwrap();
    assert_eq!(out.affected, Some(2), "insert affected");

    let mut sink = CollectSink::new(0);
    s.run("SELECT id, name, salary FROM it_emp ORDER BY id", &mut sink).await.unwrap();
    assert_eq!(sink.columns.len(), 3);
    assert!(sink.columns[0].numeric, "id numeric: {:?}", sink.columns[0]);
    assert_eq!(sink.rows[0][0], Cell::Int(1));
    assert_eq!(sink.rows[0][1], Cell::Text("Ann".into()));
    assert!(matches!(&sink.rows[0][2], Cell::Dec(d) if d.starts_with("1234.5")), "{:?}", sink.rows[0][2]);
    assert_eq!(sink.rows[1][2], Cell::Null);

    let mut sink = CollectSink::new(1);
    s.run("SELECT id FROM it_emp ORDER BY id", &mut sink).await.unwrap();
    assert!(sink.truncated && sink.rows.len() == 1);

    // manual commit: delete then rollback keeps rows
    s.set_autocommit(false).await.unwrap();
    s.exec("DELETE FROM it_emp").await.unwrap();
    s.rollback().await.unwrap();
    assert_eq!(rows(&s, "SELECT COUNT(*) FROM it_emp").await[0][0], Cell::Int(2));
    s.set_autocommit(true).await.unwrap();

    // after a truncated read the session must still be usable
    assert_eq!(rows(&s, "SELECT COUNT(*) FROM it_emp").await.len(), 1);

    let schemas = meta::list_schemas(s.as_ref()).await.unwrap();
    assert!(schemas.iter().any(|x| x.eq_ignore_ascii_case(schema)), "{schemas:?}");
    let objs = meta::list_objects(s.as_ref(), schema).await.unwrap();
    let t = objs.iter().find(|o| o.name.eq_ignore_ascii_case("it_emp")).expect("table listed");
    assert_eq!(t.kind, "table");
    let cols = meta::columns(s.as_ref(), schema, &t.name).await.unwrap();
    assert_eq!(cols.len(), 3);
    assert!(cols[0].primary_key, "{cols:?}");
    let ddl = meta::ddl(s.as_ref(), schema, &t.name, "table").await.unwrap();
    assert!(ddl.to_lowercase().contains("it_emp"), "{ddl}");
    eprintln!("version: {}", s.server_version().await.unwrap());
    s.exec("DROP TABLE it_emp").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_postgres() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"postgres","host":"127.0.0.1","port":55432,"database":"test","username":"postgres"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let r = rows(&s, "select 1::int8, true, 'x'::text, now(), '\\xcafe'::bytea, '{\"a\":1}'::jsonb, 1.50::numeric, array[1,2], interval '1 day'").await;
    assert_eq!(r[0][0], Cell::Int(1));
    assert_eq!(r[0][1], Cell::Bool(true));
    assert!(matches!(r[0][3], Cell::DateTime(_)));
    assert_eq!(r[0][4], Cell::Bytes(vec![0xCA, 0xFE]));
    assert_eq!(r[0][7], Cell::Text("{1,2}".into()));
    scenario(
        s,
        "public",
        "CREATE TABLE it_emp (id int primary key, name text not null, salary numeric(10,2))",
        "INSERT INTO it_emp VALUES (1,'Ann',1234.5),(2,'Bob',NULL)",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn it_mysql() {
    if !enabled() {
        return;
    }
    for port in [53306, 53307] {
        let p = profile(serde_json::json!({"engine":"mysql","host":"127.0.0.1","port":port,"database":"test","username":"root"}));
        let s = connect(&p, PW, "").await.unwrap_or_else(|e| panic!("port {port}: {e:#}")).session;
        let r = rows(&s, "select 1, 'x', now(), x'CAFE', 1.5, null").await;
        assert_eq!(r[0][0], Cell::Int(1));
        assert_eq!(r[0][3], Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(r[0][5], Cell::Null);
        scenario(
            s,
            "test",
            "CREATE TABLE it_emp (id int primary key, name varchar(50) not null, salary decimal(10,2))",
            "INSERT INTO it_emp VALUES (1,'Ann',1234.5),(2,'Bob',NULL)",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn it_mssql() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"mssql","host":"127.0.0.1","port":51433,"database":"master","username":"sa"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let r = rows(&s, "select cast(1 as bigint), N'ünï', getdate(), 0xCAFE, cast(1.5 as decimal(5,2)), newid()").await;
    assert_eq!(r[0][0], Cell::Int(1));
    assert_eq!(r[0][1], Cell::Text("ünï".into()));
    assert!(matches!(r[0][2], Cell::DateTime(_)));
    scenario(
        s,
        "dbo",
        "CREATE TABLE it_emp (id int primary key, name nvarchar(50) not null, salary decimal(10,2))",
        "INSERT INTO it_emp VALUES (1,'Ann',1234.5),(2,'Bob',NULL)",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn it_oracle() {
    if !enabled() || std::env::var("DBFORGE_IT_ORACLE").is_err() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"oracle","host":"127.0.0.1","port":51521,"username":"test",
        "oracle":{"method":"direct","service":"FREEPDB1"}}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let r = rows(&s, "select 1, 'x', sysdate, hextoraw('CAFE'), 1.5, null from dual").await;
    assert_eq!(r[0][0], Cell::Int(1));
    assert_eq!(r[0][3], Cell::Bytes(vec![0xCA, 0xFE]));
    assert_eq!(r[0][4], Cell::Dec("1.5".into()));
    scenario(
        s,
        "TEST",
        "CREATE TABLE it_emp (id number(10) primary key, name varchar2(50) not null, salary number(10,2))",
        "INSERT INTO it_emp SELECT 1,'Ann',1234.5 FROM dual UNION ALL SELECT 2,'Bob',NULL FROM dual",
    )
    .await;
}
