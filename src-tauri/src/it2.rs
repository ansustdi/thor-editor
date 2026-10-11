//! Phase 2 integration tests (ClickHouse, Redis, MongoDB, Neo4j, JanusGraph, SSH, import, FKs).
//! Run with:  DBFORGE_IT=1 cargo test it_ -- --test-threads=1

use crate::drivers::{connect, CollectSink, Session};
use crate::meta;
use crate::model::{Cell, ConnectionProfile};
use std::sync::Arc;

const PW: &str = "Passw0rd!";

fn enabled() -> bool {
    std::env::var("DBFORGE_IT").is_ok()
}

fn profile(mut v: serde_json::Value) -> ConnectionProfile {
    v["id"] = "it".into();
    serde_json::from_value(v).unwrap()
}

async fn rows(s: &Arc<dyn Session>, sql: &str) -> Vec<Vec<Cell>> {
    let mut sink = CollectSink::new(0);
    s.run(sql, &mut sink).await.unwrap_or_else(|e| panic!("{sql}: {e:#}"));
    sink.rows
}

#[tokio::test(flavor = "multi_thread")]
async fn it_clickhouse() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"clickhouse","host":"127.0.0.1","port":58123,"database":"test","username":"test"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let r = rows(&s, "select toUInt64(18446744073709551), 'x', now(), toDecimal64(1.5, 2), [1,2], NULL").await;
    assert_eq!(r[0][0], Cell::Int(18446744073709551));
    assert!(matches!(r[0][2], Cell::DateTime(_)));
    assert_eq!(r[0][4], Cell::Text("[1,2]".into()));
    assert_eq!(r[0][5], Cell::Null);
    let _ = s.exec("DROP TABLE IF EXISTS it_emp").await;
    s.exec("CREATE TABLE it_emp (id UInt32, name String, salary Nullable(Decimal(10,2))) ENGINE = MergeTree ORDER BY id")
        .await
        .unwrap();
    let mut sink = CollectSink::new(0);
    let out = s.run("INSERT INTO it_emp VALUES (1,'Ann',1234.5),(2,'Bob',NULL)", &mut sink).await.unwrap();
    assert_eq!(out.affected, Some(2));
    let mut sink = CollectSink::new(1);
    s.run("SELECT * FROM it_emp ORDER BY id", &mut sink).await.unwrap();
    assert!(sink.truncated && sink.columns[0].numeric);
    let objs = meta::list_objects(s.as_ref(), "test").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "it_emp" && o.kind == "table"));
    let cols = meta::columns(s.as_ref(), "test", "it_emp").await.unwrap();
    assert!(cols[0].primary_key && cols[2].nullable, "{cols:?}");
    assert!(meta::ddl(s.as_ref(), "test", "it_emp", "table").await.unwrap().contains("MergeTree"));
    assert!(s.run("SELEC 1", &mut CollectSink::new(0)).await.is_err());
    eprintln!("{}", s.server_version().await.unwrap());
    s.exec("DROP TABLE it_emp").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_redis() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"redis","host":"127.0.0.1","port":56379,"database":"2"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    s.exec("FLUSHDB").await.unwrap();
    s.exec(r#"SET "greeting key" "hello world""#).await.unwrap();
    s.exec("HSET user:1 name Ann age 30").await.unwrap();
    s.exec("RPUSH q a b c").await.unwrap();
    s.exec("ZADD board 10 x 20 y").await.unwrap();
    assert_eq!(rows(&s, r#"GET "greeting key""#).await[0][0], Cell::Text("hello world".into()));
    assert_eq!(rows(&s, "HGETALL user:1").await.len(), 2);
    assert_eq!(rows(&s, "LRANGE q 0 -1").await.len(), 3);
    let z = rows(&s, "ZRANGE board 0 -1 WITHSCORES").await;
    assert_eq!(z[1][0], Cell::Text("y".into()));
    assert_eq!(meta::current_schema(s.as_ref()).await.unwrap(), "db2");
    let objs = meta::list_objects(s.as_ref(), "db2").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "user:1" && o.kind == "hash"), "{objs:?}");
    assert!(meta::list_schemas(s.as_ref()).await.unwrap().contains(&"db2".to_string()));
    let ddl = meta::ddl(s.as_ref(), "db2", "q", "list").await.unwrap();
    assert!(ddl.contains("LLEN): 3"), "{ddl}");
    assert!(s.run("NOSUCHCMD", &mut CollectSink::new(0)).await.is_err());
    eprintln!("{}", s.server_version().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn it_mongo() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"mongodb","host":"127.0.0.1","port":57017,"database":"ittest","username":"root"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let _ = s.exec("db.emp.drop()").await;
    let mut sink = CollectSink::new(0);
    let out = s
        .run(
            r#"db.emp.insertMany([{_id: 1, name: "Ann", salary: 1234.5, tags: ["a"], at: ISODate("2024-01-02T03:04:05Z")}, {_id: 2, name: 'Bob', extra: {x: 1}}])"#,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(out.affected, Some(2));
    let mut sink = CollectSink::new(0);
    s.run("db.emp.find({}).sort({_id: 1})", &mut sink).await.unwrap();
    let names: Vec<&str> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "_id");
    assert!(names.contains(&"extra") && names.contains(&"tags"));
    assert_eq!(sink.rows.len(), 2);
    let at = names.iter().position(|n| *n == "at").unwrap();
    assert!(matches!(&sink.rows[0][at], Cell::DateTime(d) if d.starts_with("2024-01-02")), "{:?}", sink.rows[0][at]);
    assert_eq!(rows(&s, "db.emp.countDocuments({salary: {$gt: 1000}})").await[0][0], Cell::Int(1));
    let agg = rows(&s, r#"db.getSiblingDB("ittest").getCollection("emp").aggregate([{$group: {_id: null, n: {$sum: 1}}}])"#).await;
    assert_eq!(agg[0][1], Cell::Int(2));
    let mut sink = CollectSink::new(0);
    let out = s.run(r#"db.emp.updateMany({}, {$set: {ok: true}})"#, &mut sink).await.unwrap();
    assert_eq!(out.affected, Some(2));
    let mut sink = CollectSink::new(1);
    s.run("db.emp.find()", &mut sink).await.unwrap();
    assert!(sink.truncated);
    assert!(meta::list_schemas(s.as_ref()).await.unwrap().contains(&"ittest".to_string()));
    let objs = meta::list_objects(s.as_ref(), "ittest").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "emp" && o.kind == "collection"));
    let cols = meta::columns(s.as_ref(), "ittest", "emp").await.unwrap();
    assert!(cols.iter().any(|c| c.name == "_id" && c.primary_key));
    assert!(meta::ddl(s.as_ref(), "ittest", "emp", "collection").await.unwrap().contains("createIndex"));
    eprintln!("{}", s.server_version().await.unwrap());
    s.exec("db.emp.drop()").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_neo4j() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"neo4j","host":"127.0.0.1","port":57687,"username":"neo4j"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    s.exec("MATCH (n:ItPerson) DETACH DELETE n").await.unwrap();
    s.exec("CREATE (a:ItPerson {name:'Ann', age: 30})-[:KNOWS {since: 2020}]->(b:ItPerson {name:'Bob'})").await.unwrap();
    let mut sink = CollectSink::new(0);
    s.run("MATCH (a:ItPerson)-[r:KNOWS]->(b) RETURN a, r, b.name AS friend, 1 AS one", &mut sink).await.unwrap();
    let names: Vec<&str> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["a", "r", "friend", "one"]);
    let Cell::Text(node) = &sink.rows[0][0] else { panic!("node cell") };
    let v: serde_json::Value = serde_json::from_str(node).unwrap();
    assert_eq!(v["~labels"][0], "ItPerson");
    assert_eq!(v["name"], "Ann");
    let Cell::Text(rel) = &sink.rows[0][1] else { panic!("rel cell") };
    assert!(rel.contains("\"~type\":\"KNOWS\""), "{rel}");
    assert_eq!(sink.rows[0][3], Cell::Int(1));
    let mut sink = CollectSink::new(0);
    s.run("MATCH (n:ItPerson) WHERE n.name = 'nobody' RETURN n.name AS name", &mut sink).await.unwrap();
    assert_eq!(sink.columns.len(), 1);
    let objs = meta::list_objects(s.as_ref(), "neo4j").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "ItPerson" && o.kind == "label"));
    assert!(objs.iter().any(|o| o.name == "KNOWS" && o.kind == "relationship"));
    let cols = meta::columns(s.as_ref(), "neo4j", "ItPerson").await.unwrap();
    assert!(cols.iter().any(|c| c.name == "age"));
    eprintln!("{}", s.server_version().await.unwrap());
    s.exec("MATCH (n:ItPerson) DETACH DELETE n").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_janusgraph() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"janusgraph","host":"127.0.0.1","port":58182}));
    let s = connect(&p, "", "").await.unwrap().session;
    eprintln!("{}", s.server_version().await.unwrap());
    s.exec("g.V().hasLabel('ItPerson').drop().iterate()").await.unwrap();
    s.exec("a = g.addV('ItPerson').property('name','Ann').property('age',30).next(); b = g.addV('ItPerson').property('name','Bob').next(); g.addE('itKnows').from(a).to(b).property('since',2020).iterate()")
        .await
        .unwrap();
    let r = rows(&s, "g.V().hasLabel('ItPerson').has('name','Ann').outE('itKnows').inV().path()").await;
    let Cell::Text(path) = &r[0][0] else { panic!("path cell") };
    let v: serde_json::Value = serde_json::from_str(path).unwrap();
    // Gremlin Server detaches path elements to references (id + label, no properties).
    assert_eq!(v["~path"][0]["~labels"][0], "ItPerson");
    assert_eq!(v["~path"].as_array().unwrap().len(), 2);
    assert_eq!(v["~rels"][0]["~type"], "itKnows");
    assert_eq!(v["~rels"][0]["~start"], v["~path"][0]["~id"]);
    let ids: Vec<String> = v["~path"].as_array().unwrap().iter().map(|n| n["~id"].as_str().unwrap().to_string()).collect();
    let edges = s.graph_edges(&ids, 100).await.unwrap();
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0]["~type"], "itKnows");
    assert!(s.graph_edges(&ids[..1], 100).await.unwrap().is_empty());
    let mut sink = CollectSink::new(0);
    s.run("g.V().hasLabel('ItPerson').order().by('name').project('name','deg').by('name').by(bothE().count())", &mut sink).await.unwrap();
    let names: Vec<&str> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["name", "deg"]);
    assert_eq!(sink.rows[0], vec![Cell::Text("Ann".into()), Cell::Int(1)]);
    let mut sink = CollectSink::new(0);
    s.run("g.V().hasLabel('ItPerson').has('name','nobody')", &mut sink).await.unwrap();
    assert!(sink.rows.is_empty() && sink.columns.len() == 1);
    assert!(s.run("g.V(", &mut CollectSink::new(0)).await.is_err());
    let objs = meta::list_objects(s.as_ref(), "g").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "ItPerson" && o.kind == "label"), "{objs:?}");
    assert!(objs.iter().any(|o| o.name == "itKnows" && o.kind == "relationship"));
    let cols = meta::columns(s.as_ref(), "g", "ItPerson").await.unwrap();
    assert!(cols.iter().any(|c| c.name == "age"), "{cols:?}");
    eprintln!("{}", meta::ddl(s.as_ref(), "g", "ItPerson", "label").await.unwrap());
    s.exec("g.V().hasLabel('ItPerson').drop().iterate()").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_ssh_tunnel_to_postgres() {
    if !enabled() {
        return;
    }
    // The sshd container reaches postgres by its compose service name.
    let p = profile(serde_json::json!({"engine":"postgres","host":"postgres","port":5432,"database":"test","username":"postgres",
        "ssh":{"enabled":true,"host":"127.0.0.1","port":52222,"user":"tunnel"}}));
    let c = connect(&p, PW, PW).await.unwrap();
    assert!(c.tunnel.is_some());
    assert_eq!(rows(&c.session, "select 42").await[0][0], Cell::Int(42));
}

#[tokio::test(flavor = "multi_thread")]
async fn it_import_and_fks_postgres() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"postgres","host":"127.0.0.1","port":55432,"database":"test","username":"postgres"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    let _ = s.exec("DROP TABLE IF EXISTS it_child, it_parent, it_imp").await;
    s.exec("CREATE TABLE it_parent (a int, b int, primary key (a, b))").await.unwrap();
    s.exec("CREATE TABLE it_child (id int primary key, pa int, pb int, foreign key (pa, pb) references it_parent (a, b))")
        .await
        .unwrap();
    let fks = meta::foreign_keys(s.as_ref(), "public").await.unwrap();
    let fk = fks.iter().find(|f| f.table == "it_child").expect("fk");
    assert_eq!(fk.columns, vec!["pa", "pb"]);
    assert_eq!((fk.ref_table.as_str(), fk.ref_columns.clone()), ("it_parent", vec!["a".to_string(), "b".into()]));

    let dir = std::env::temp_dir().join(format!("it-imp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("in.csv");
    let mut csv = String::from("id,name,price\n");
    for i in 0..1234 {
        csv.push_str(&format!("{i},\"n,{i}\",{i}.5\n"));
    }
    std::fs::write(&f, csv).unwrap();
    let src = crate::importer::SourceOptions::default();
    let pv = crate::importer::preview(&f, &src, Some(crate::drivers::Dialect::Postgres)).unwrap();
    let t = crate::importer::TargetOptions {
        table: "it_imp".into(),
        mapping: pv.columns.clone(),
        create: true,
        types: pv.types.clone(),
        batch_size: 500,
        ..Default::default()
    };
    let n = crate::importer::run(s.as_ref(), &f, &src, &t, |_| {}).await.unwrap();
    assert_eq!(n, 1234);
    let r = rows(&s, "select count(*), max(name) from it_imp").await;
    assert_eq!(r[0][0], Cell::Int(1234));
    s.exec("DROP TABLE it_child, it_parent, it_imp").await.unwrap();
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_clickhouse_errors_and_camel_case() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"clickhouse","host":"127.0.0.1","port":58123,"database":"default","username":"test"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    // what the old formatter produced
    let e = s.run("SELECT TOYEAR(TODATE('2026-09-01'))", &mut CollectSink::new(0)).await.unwrap_err();
    let msg = format!("{e:#}");
    eprintln!("error: {msg}");
    assert!(msg.starts_with("Code: 46. DB::Exception:"), "{msg}");
    assert!(!msg.contains("[]") && !msg.contains("(version"), "{msg}");
    // error raised while rows are already streaming (throwIf on a later block)
    let e = s
        .run("SELECT number, throwIf(number = 70000, 'late failure') FROM numbers(100000) SETTINGS max_block_size = 1000", &mut CollectSink::new(0))
        .await
        .unwrap_err();
    eprintln!("mid-stream: {:#}", e);
    assert!(format!("{e:#}").contains("late failure"));
    // the formatter's output for ClickHouse keeps camelCase functions and runs
    let r = rows(&s, "SELECT toYear(toDate('2026-09-01')) AS y, sumIf(number, number < 3) AS s FROM numbers(10) SETTINGS join_use_nulls = 1").await;
    assert_eq!(r[0][0], Cell::Int(2026));
}

#[tokio::test(flavor = "multi_thread")]
async fn it_elasticsearch() {
    if !enabled() {
        return;
    }
    let p = profile(serde_json::json!({"engine":"elasticsearch","host":"127.0.0.1","port":59200,"username":"elastic"}));
    let s = connect(&p, PW, "").await.unwrap().session;
    assert!(s.server_version().await.unwrap().starts_with("Elasticsearch 8"));
    let _ = s.exec("DELETE it-people").await;
    s.exec("PUT it-people\n{\"mappings\":{\"properties\":{\"name\":{\"type\":\"text\",\"fields\":{\"keyword\":{\"type\":\"keyword\"}}},\"age\":{\"type\":\"integer\"},\"city\":{\"type\":\"keyword\"},\"born\":{\"type\":\"date\"}}}}")
        .await
        .unwrap();
    // 1500 docs: more than one SQL page (fetch size 1000).
    let mut bulk = String::from("POST it-people/_bulk?refresh=true\n");
    for i in 0..1500 {
        let city = if i % 3 == 0 { "tokyo" } else { "ulaanbaatar" };
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"name\":\"person {i}\",\"age\":{},\"city\":\"{city}\",\"born\":\"2000-01-01\"}}\n", 20 + i % 40));
    }
    s.exec(&bulk).await.unwrap();

    // console search → hits as rows
    let mut sink = CollectSink::new(0);
    s.run("GET it-people/_search\n{\"query\":{\"term\":{\"city\":\"tokyo\"}},\"size\":5,\"sort\":[{\"age\":\"asc\"}]}", &mut sink).await.unwrap();
    let cols: Vec<&str> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&cols[..1], &["_id"]);
    assert!(cols.contains(&"name") && cols.contains(&"city"), "{cols:?}");
    assert_eq!(sink.rows.len(), 5);

    // aggregation → bucket rows
    let mut sink = CollectSink::new(0);
    s.run("POST it-people/_search\n{\"size\":0,\"aggs\":{\"by_city\":{\"terms\":{\"field\":\"city\"},\"aggs\":{\"avg_age\":{\"avg\":{\"field\":\"age\"}}}}}}", &mut sink).await.unwrap();
    let cols: Vec<&str> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, vec!["by_city", "doc_count", "avg_age"]);
    assert_eq!(sink.rows[0][0], Cell::Text("ulaanbaatar".into()));
    assert_eq!(sink.rows[0][1], Cell::Int(1000));

    // SQL with cursor paging
    let r = rows(&s, "SELECT name, age FROM \"it-people\" ORDER BY age").await;
    assert_eq!(r.len(), 1500);
    let r = rows(&s, "SELECT city, COUNT(*) AS n FROM \"it-people\" GROUP BY city ORDER BY city;").await;
    assert_eq!(r[0], vec![Cell::Text("tokyo".into()), Cell::Int(500)]);
    // limited fetch closes the cursor early
    let mut sink = CollectSink::new(10);
    s.run("SELECT * FROM \"it-people\"", &mut sink).await.unwrap();
    assert_eq!(sink.rows.len(), 10);

    // _cat → table, HEAD, key/value fallback, readable errors
    let r = rows(&s, "GET _cat/indices/it-people?h=index,docs.count").await;
    assert_eq!(r[0][0], Cell::Text("it-people".into()));
    assert_eq!(rows(&s, "HEAD it-people").await[0][1], Cell::Bool(true));
    let r = rows(&s, "GET _cluster/health").await;
    assert!(r.iter().any(|row| row[0] == Cell::Text("cluster_name".into())));
    let e = s.run("GET nope-index/_search", &mut CollectSink::new(0)).await.unwrap_err();
    assert!(format!("{e:#}").contains("index_not_found_exception"), "{e:#}");
    let e = s.run("SELECT * FROM nope", &mut CollectSink::new(0)).await.unwrap_err();
    assert!(format!("{e:#}").contains("Unknown index"), "{e:#}");

    // metadata
    let objs = meta::list_objects(s.as_ref(), "docker-cluster").await.unwrap();
    assert!(objs.iter().any(|o| o.name == "it-people" && o.kind == "es index"), "{objs:?}");
    let cols = meta::columns(s.as_ref(), "docker-cluster", "it-people").await.unwrap();
    assert!(cols.iter().any(|c| c.name == "name.keyword" && c.data_type == "keyword"), "{cols:?}");
    let ddl = meta::ddl(s.as_ref(), "docker-cluster", "it-people", "es index").await.unwrap();
    assert!(ddl.starts_with("PUT it-people\n") && !ddl.contains("creation_date"), "{ddl}");
    // the DDL is runnable as-is
    s.exec("DELETE it-people").await.unwrap();
    s.exec(&ddl).await.unwrap();
    s.exec("DELETE it-people").await.unwrap();
}
