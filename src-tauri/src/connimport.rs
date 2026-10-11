//! Import saved connections from Toad for Oracle, DBeaver and DataGrip.
//! Passwords are never imported (the source tools encrypt them).

use crate::model::{ConnectionProfile, Engine};
use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn blank(engine: Engine) -> ConnectionProfile {
    serde_json::from_value(serde_json::json!({ "id": uuid::Uuid::new_v4().to_string(), "engine": engine })).unwrap()
}

/// Parse a JDBC (or native) URL into a profile. Unknown schemes return None.
pub fn from_url(url: &str) -> Option<ConnectionProfile> {
    let u = url.trim();
    let rest = u.strip_prefix("jdbc:").unwrap_or(u);
    let (scheme, body) = rest.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    let engine = match scheme.as_str() {
        "postgresql" | "postgres" | "pgsql" => Engine::Postgres,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "sqlserver" | "jtds" => Engine::Mssql,
        "oracle" => Engine::Oracle,
        "sqlite" => Engine::Sqlite,
        "clickhouse" | "ch" => Engine::Clickhouse,
        "mongodb" | "mongodb+srv" => Engine::Mongodb,
        "neo4j" | "bolt" | "neo4j+s" | "bolt+s" => Engine::Neo4j,
        "redis" | "rediss" => Engine::Redis,
        "elasticsearch" | "es" | "opensearch" => Engine::Elasticsearch,
        _ => return None,
    };
    let mut p = blank(engine);
    match engine {
        Engine::Sqlite => {
            p.database = body.to_string();
            return Some(p);
        }
        Engine::Mongodb => {
            p.params.insert("uri".into(), u.trim_start_matches("jdbc:").to_string());
            if let Ok(parsed) = url::Url::parse(u.trim_start_matches("jdbc:")) {
                p.host = parsed.host_str().unwrap_or_default().to_string();
                p.port = parsed.port().unwrap_or(0);
                p.username = parsed.username().to_string();
                p.database = parsed.path().trim_start_matches('/').to_string();
            }
            return Some(p);
        }
        Engine::Oracle => {
            // thin:@//host:port/service | thin:@host:port:SID | thin:@ALIAS | thin:@(DESCRIPTION=...)
            let at = body.split_once('@').map(|x| x.1).unwrap_or(body);
            if let Some(s) = at.strip_prefix("//") {
                let (hp, svc) = s.split_once('/').unwrap_or((s, ""));
                let (h, port) = hp.split_once(':').unwrap_or((hp, "1521"));
                p.host = h.into();
                p.port = port.parse().unwrap_or(1521);
                p.oracle.method = "direct".into();
                p.oracle.service_type = "service".into();
                p.oracle.service = svc.split('?').next().unwrap_or("").into();
            } else if at.starts_with('(') {
                let grab = |k: &str| {
                    let low = at.to_ascii_uppercase();
                    low.find(&format!("({k}=")).map(|i| {
                        let v = &at[i + k.len() + 2..];
                        v[..v.find(')').unwrap_or(v.len())].trim().to_string()
                    })
                };
                p.host = grab("HOST").unwrap_or_default();
                p.port = grab("PORT").and_then(|x| x.parse().ok()).unwrap_or(1521);
                p.oracle.method = "direct".into();
                if let Some(svc) = grab("SERVICE_NAME") {
                    p.oracle.service_type = "service".into();
                    p.oracle.service = svc;
                } else {
                    p.oracle.service_type = "sid".into();
                    p.oracle.service = grab("SID").unwrap_or_default();
                }
            } else {
                let parts: Vec<&str> = at.split(':').collect();
                if parts.len() == 3 {
                    p.host = parts[0].into();
                    p.port = parts[1].parse().unwrap_or(1521);
                    p.oracle.method = "direct".into();
                    p.oracle.service_type = "sid".into();
                    p.oracle.service = parts[2].into();
                } else {
                    p.oracle.method = "tns".into();
                    p.oracle.tns_alias = at.to_string();
                }
            }
            return Some(p);
        }
        Engine::Mssql => {
            // //host[\instance][:port][;databaseName=x;...]
            let s = body.trim_start_matches("sqlserver:").trim_start_matches("//");
            let mut parts = s.split(';');
            let hp = parts.next().unwrap_or("");
            let (h, port) = hp.split_once(':').unwrap_or((hp, ""));
            p.host = h.split('\\').next().unwrap_or(h).into();
            p.port = port.parse().unwrap_or(0);
            for kv in parts {
                if let Some((k, v)) = kv.split_once('=') {
                    match k.to_ascii_lowercase().as_str() {
                        "databasename" | "database" => p.database = v.into(),
                        "user" => p.username = v.into(),
                        "integratedsecurity" if v.eq_ignore_ascii_case("true") => p.auth = "windows".into(),
                        _ => {}
                    }
                }
            }
            return Some(p);
        }
        _ => {}
    }
    // host-style URLs
    let parsed = url::Url::parse(&format!("x://{}", body.trim_start_matches("//"))).ok()?;
    p.host = parsed.host_str().unwrap_or_default().to_string();
    p.port = parsed.port().unwrap_or(0);
    p.username = parsed.username().to_string();
    p.database = parsed.path().trim_start_matches('/').to_string();
    if engine == Engine::Neo4j && scheme.ends_with("+s") {
        p.ssl_mode = "verify-full".into();
    }
    Some(p)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownSource {
    pub kind: String,
    pub label: String,
    pub path: String,
}

/// Default locations of other tools' connection files that exist on this machine.
pub fn known_sources() -> Vec<KnownSource> {
    let mut out = vec![];
    let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else { return out };
    let toad = appdata.join("Quest Software").join("Toad for Oracle");
    if let Ok(rd) = std::fs::read_dir(&toad) {
        let mut vers: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        vers.sort();
        for v in vers.into_iter().rev() {
            let f = v.join("User Files").join("Connections.xml");
            if f.exists() {
                out.push(KnownSource {
                    kind: "toad".into(),
                    label: format!("Toad for Oracle {}", v.file_name().unwrap_or_default().to_string_lossy()),
                    path: f.to_string_lossy().into(),
                });
            }
        }
    }
    let dbeaver = appdata.join("DBeaverData").join("workspace6").join("General").join(".dbeaver").join("data-sources.json");
    if dbeaver.exists() {
        out.push(KnownSource { kind: "dbeaver".into(), label: "DBeaver".into(), path: dbeaver.to_string_lossy().into() });
    }
    if let Ok(rd) = std::fs::read_dir(appdata.join("JetBrains")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let f = e.path().join("options").join("dataSources.xml");
            if name.starts_with("DataGrip") && f.exists() {
                out.push(KnownSource { kind: "datagrip".into(), label: name, path: f.to_string_lossy().into() });
            }
        }
    }
    out
}

pub fn parse(kind: &str, path: &Path) -> Result<Vec<ConnectionProfile>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = text.trim_start_matches('\u{feff}');
    match kind {
        "toad" => parse_toad(text),
        "dbeaver" => parse_dbeaver(text),
        "datagrip" => {
            let local = path.with_file_name("dataSources.local.xml");
            let local = std::fs::read_to_string(local).unwrap_or_default();
            parse_datagrip(text, &local)
        }
        other => bail!("unknown source '{other}'"),
    }
}

// ---------- Toad ----------

fn toad_color(v: &str) -> String {
    // Toad stores a Delphi TColor (BGR integer) or a color name.
    if let Ok(n) = v.trim().parse::<i64>() {
        if n < 0 || n == 0x1FFFFFFF {
            return String::new();
        }
        let (r, g, b) = (n & 0xFF, (n >> 8) & 0xFF, (n >> 16) & 0xFF);
        return format!("#{r:02x}{g:02x}{b:02x}");
    }
    match v.trim().to_ascii_lowercase().trim_start_matches("cl") {
        "red" | "maroon" => "#dc2626".into(),
        "green" | "lime" => "#16a34a".into(),
        "blue" | "navy" => "#2563eb".into(),
        "yellow" | "olive" => "#ca8a04".into(),
        "purple" | "fuchsia" => "#9333ea".into(),
        "gray" | "grey" | "silver" => "#6b7280".into(),
        _ => String::new(),
    }
}

pub fn parse_toad(xml: &str) -> Result<Vec<ConnectionProfile>> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut out = vec![];
    let mut folders: Vec<String> = vec![];
    let mut stack: Vec<String> = vec![];
    let mut cur: Option<HashMap<String, String>> = None;
    let mut service_attr = false;
    let homes = crate::oracle_env::list_homes();
    loop {
        match r.read_event()? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Connection" {
                    cur = Some(HashMap::new());
                    service_attr = false;
                } else if cur.is_none() && (name == "Folder" || name == "Group") {
                    let n = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref().eq_ignore_ascii_case(b"name"))
                        .map(|a| String::from_utf8_lossy(&a.value).to_string())
                        .unwrap_or_default();
                    folders.push(n);
                } else if name == "ConnectionType" {
                    service_attr = e
                        .attributes()
                        .flatten()
                        .any(|a| a.key.as_ref() == b"ServiceName" && a.value.as_ref().eq_ignore_ascii_case(b"true"));
                }
                stack.push(name);
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                stack.pop();
                if name == "Connection" {
                    if let Some(m) = cur.take() {
                        out.push(toad_profile(&m, service_attr, folders.last().cloned().unwrap_or_default(), &homes));
                    }
                } else if cur.is_none() && (name == "Folder" || name == "Group") {
                    folders.pop();
                }
            }
            Event::Text(t) => {
                if let (Some(m), Some(tag)) = (cur.as_mut(), stack.last()) {
                    m.insert(tag.clone(), t.decode().map(|s| s.to_string()).unwrap_or_default());
                }
            }
            Event::CData(t) => {
                if let (Some(m), Some(tag)) = (cur.as_mut(), stack.last()) {
                    m.insert(tag.clone(), String::from_utf8_lossy(&t).trim().to_string());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn toad_profile(m: &HashMap<String, String>, service: bool, folder: String, homes: &[crate::oracle_env::OracleHome]) -> ConnectionProfile {
    let g = |k: &str| m.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut p = blank(Engine::Oracle);
    p.username = g("User");
    p.folder = folder;
    p.favorite = g("Favorite").eq_ignore_ascii_case("true");
    p.color = toad_color(&g("Color"));
    let mode = g("ConnectionMode").to_ascii_lowercase();
    p.oracle.connect_as = if mode.is_empty() || mode == "normal" { "normal".into() } else { mode };
    let home = g("Home");
    if let Some(h) = homes.iter().find(|h| h.name.eq_ignore_ascii_case(&home)) {
        p.oracle.client_lib_dir = h.lib_dir.clone();
    }
    let host = g("Host");
    let ldap = g("LDAP");
    let server = g("Server");
    if !host.is_empty() {
        p.oracle.method = "direct".into();
        p.host = host;
        p.port = g("Port").parse().unwrap_or(1521);
        p.oracle.service = g("Sid");
        p.oracle.service_type = if service { "service".into() } else { "sid".into() };
    } else if !ldap.is_empty() {
        p.oracle.method = "ldap".into();
        p.oracle.ldap_alias = if server.is_empty() { ldap } else { server };
    } else {
        p.oracle.method = "tns".into();
        p.oracle.tns_alias = server;
    }
    let ssh_host = g("SSHHost");
    if !ssh_host.is_empty() {
        p.ssh.enabled = true;
        p.ssh.host = ssh_host;
        p.ssh.port = g("SSHPort").parse().unwrap_or(22);
        p.ssh.user = g("SSHUser");
        p.ssh.key_path = g("SSHPrivateKey");
    }
    let alias = g("Alias");
    if !alias.is_empty() {
        p.name = alias;
    }
    p
}

// ---------- DBeaver ----------

pub fn parse_dbeaver(json: &str) -> Result<Vec<ConnectionProfile>> {
    let v: serde_json::Value = serde_json::from_str(json).context("invalid data-sources.json")?;
    let mut out = vec![];
    let Some(conns) = v["connections"].as_object() else { return Ok(out) };
    for (_id, c) in conns {
        let cfg = &c["configuration"];
        let s = |x: &serde_json::Value| x.as_str().unwrap_or("").to_string();
        let provider = format!("{} {}", s(&c["provider"]), s(&c["driver"])).to_ascii_lowercase();
        let engine = if provider.contains("mariadb") {
            Engine::Mariadb
        } else if provider.contains("mysql") {
            Engine::Mysql
        } else if provider.contains("postgres") {
            Engine::Postgres
        } else if provider.contains("oracle") {
            Engine::Oracle
        } else if provider.contains("sqlserver") || provider.contains("mssql") {
            Engine::Mssql
        } else if provider.contains("sqlite") {
            Engine::Sqlite
        } else if provider.contains("clickhouse") {
            Engine::Clickhouse
        } else if provider.contains("mongo") {
            Engine::Mongodb
        } else if provider.contains("redis") {
            Engine::Redis
        } else if provider.contains("neo4j") {
            Engine::Neo4j
        } else if provider.contains("janus") || provider.contains("gremlin") {
            Engine::Janusgraph
        } else if provider.contains("elastic") || provider.contains("opensearch") {
            Engine::Elasticsearch
        } else if let Some(p) = from_url(&s(&cfg["url"])) {
            p.engine
        } else {
            continue;
        };
        let mut p = if s(&cfg["host"]).is_empty() {
            from_url(&s(&cfg["url"])).filter(|x| x.engine == engine).unwrap_or_else(|| blank(engine))
        } else {
            blank(engine)
        };
        p.name = s(&c["name"]);
        p.folder = s(&c["folder"]);
        if !s(&cfg["host"]).is_empty() {
            p.host = s(&cfg["host"]);
        }
        if let Some(port) = cfg["port"].as_str().and_then(|x| x.parse().ok()).or(cfg["port"].as_u64().map(|x| x as u16)) {
            p.port = port;
        }
        if !s(&cfg["database"]).is_empty() {
            p.database = s(&cfg["database"]);
        }
        if !s(&cfg["user"]).is_empty() {
            p.username = s(&cfg["user"]);
        }
        if engine == Engine::Oracle {
            let props = &cfg["provider-properties"];
            let conn_type = s(&props["@dbeaver-connection-type@"]).to_ascii_uppercase();
            if conn_type == "TNS" {
                p.oracle.method = "tns".into();
                p.oracle.tns_alias = s(&cfg["database"]);
            } else if p.oracle.service.is_empty() {
                p.oracle.method = "direct".into();
                p.oracle.service = s(&cfg["database"]);
                p.oracle.service_type =
                    if s(&props["@dbeaver-sid-service@"]).eq_ignore_ascii_case("SID") { "sid".into() } else { "service".into() };
            }
            let logon = s(&props["oracle.logon-as"]).to_ascii_lowercase();
            if !logon.is_empty() {
                p.oracle.connect_as = logon;
            }
        }
        if s(&cfg["type"]).eq_ignore_ascii_case("prod") {
            p.color = "#dc2626".into();
        } else if s(&cfg["type"]).eq_ignore_ascii_case("test") {
            p.color = "#ca8a04".into();
        }
        out.push(p);
    }
    Ok(out)
}

// ---------- DataGrip ----------

fn datagrip_sources(xml: &str) -> Result<Vec<HashMap<String, String>>> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut out = vec![];
    let mut cur: Option<HashMap<String, String>> = None;
    let mut tag = String::new();
    loop {
        match r.read_event()? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "data-source" {
                    let mut m = HashMap::new();
                    for a in e.attributes().flatten() {
                        m.insert(
                            format!("@{}", String::from_utf8_lossy(a.key.as_ref())),
                            a.unescape_value().map(|v| v.to_string()).unwrap_or_default(),
                        );
                    }
                    cur = Some(m);
                }
                tag = name;
            }
            Event::Text(t) => {
                if let Some(m) = cur.as_mut() {
                    m.insert(tag.clone(), t.decode().map(|s| s.to_string()).unwrap_or_default());
                }
            }
            Event::End(e) => {
                if e.name().as_ref() == b"data-source" {
                    if let Some(m) = cur.take() {
                        out.push(m);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

pub fn parse_datagrip(xml: &str, local_xml: &str) -> Result<Vec<ConnectionProfile>> {
    let users: HashMap<String, String> = if local_xml.is_empty() {
        HashMap::new()
    } else {
        datagrip_sources(local_xml)?
            .into_iter()
            .filter_map(|m| Some((m.get("@uuid")?.clone(), m.get("user-name")?.clone())))
            .collect()
    };
    let mut out = vec![];
    for m in datagrip_sources(xml)? {
        let Some(url) = m.get("jdbc-url") else { continue };
        let Some(mut p) = from_url(url) else { continue };
        p.name = m.get("@name").cloned().unwrap_or_default();
        if let Some(u) = m.get("@uuid").and_then(|id| users.get(id)) {
            p.username = u.clone();
        }
        if let Some(g) = m.get("@group") {
            p.folder = g.clone();
        }
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdbc_urls() {
        let p = from_url("jdbc:postgresql://db.local:6543/app?ssl=true").unwrap();
        assert_eq!((p.engine, p.host.as_str(), p.port, p.database.as_str()), (Engine::Postgres, "db.local", 6543, "app"));
        let p = from_url("jdbc:oracle:thin:@//10.0.0.1:1521/ORCLPDB1").unwrap();
        assert_eq!((p.oracle.method.as_str(), p.oracle.service.as_str()), ("direct", "ORCLPDB1"));
        let p = from_url("jdbc:oracle:thin:@10.0.0.1:1521:ORCL").unwrap();
        assert_eq!((p.oracle.service_type.as_str(), p.oracle.service.as_str()), ("sid", "ORCL"));
        let p = from_url("jdbc:oracle:thin:@PROD_TNS").unwrap();
        assert_eq!((p.oracle.method.as_str(), p.oracle.tns_alias.as_str()), ("tns", "PROD_TNS"));
        let p = from_url("jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h1)(PORT=1522))(CONNECT_DATA=(SERVICE_NAME=svc)))").unwrap();
        assert_eq!((p.host.as_str(), p.port, p.oracle.service.as_str()), ("h1", 1522, "svc"));
        let p = from_url("jdbc:sqlserver://sql1:1444;databaseName=Sales;integratedSecurity=true").unwrap();
        assert_eq!((p.host.as_str(), p.port, p.database.as_str(), p.auth.as_str()), ("sql1", 1444, "Sales", "windows"));
        let p = from_url("jdbc:sqlite:C:\\data\\a.db").unwrap();
        assert_eq!(p.database, "C:\\data\\a.db");
        assert_eq!(from_url("jdbc:mariadb://m:3307/x").unwrap().engine, Engine::Mariadb);
        assert!(from_url("jdbc:unknown://x").is_none());
    }

    #[test]
    fn toad_xml() {
        let xml = r#"<?xml version="1.0"?><ToadOracle><FileVersion Encryption="AES">1</FileVersion>
          <ConnectionHierarchy><DbPlatform name="Oracle"><Connections>
            <Connection type="Oracle"><User><![CDATA[SCOTT]]></User><Server>PROD</Server><Home>NoSuchHome</Home>
              <Password>ENCRYPTED</Password><Favorite>True</Favorite><Alias/><Host/><Sid/><Port>1521</Port><LDAP/>
              <SSHHost/><ConnectionType ServiceName="True">TNS</ConnectionType><Color>255</Color><ConnectionMode>SYSDBA</ConnectionMode></Connection>
            <Connection type="Oracle"><User><![CDATA[HR]]></User><Server>h:1521/PDB1</Server><Host>h</Host><Sid>PDB1</Sid><Port>1522</Port>
              <ConnectionType ServiceName="True">Direct</ConnectionType><SSHHost>jump</SSHHost><SSHPort>2222</SSHPort><SSHUser>ops</SSHUser></Connection>
          </Connections></DbPlatform></ConnectionHierarchy></ToadOracle>"#;
        let v = parse_toad(xml).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].username.as_str(), v[0].oracle.method.as_str(), v[0].oracle.tns_alias.as_str()), ("SCOTT", "tns", "PROD"));
        assert!(v[0].favorite);
        assert_eq!(v[0].color, "#ff0000");
        assert_eq!(v[0].oracle.connect_as, "sysdba");
        assert_eq!((v[1].host.as_str(), v[1].port, v[1].oracle.service.as_str()), ("h", 1522, "PDB1"));
        assert_eq!(v[1].oracle.service_type, "service");
        assert!(v[1].ssh.enabled && v[1].ssh.port == 2222 && v[1].ssh.user == "ops");
    }

    #[test]
    fn dbeaver_json() {
        let j = r#"{"folders":{"Prod":{}},"connections":{
          "postgres-jdbc-1":{"provider":"postgresql","driver":"postgres-jdbc","name":"Main PG","folder":"Prod",
            "configuration":{"host":"pg1","port":"5433","database":"app","user":"alice","type":"prod"}},
          "oracle_thin-2":{"provider":"oracle","driver":"oracle_thin","name":"Ora",
            "configuration":{"host":"o1","port":"1521","database":"ORCL","provider-properties":{"@dbeaver-sid-service@":"SID","oracle.logon-as":"SYSDBA"}}},
          "x-3":{"provider":"generic","driver":"sqlite_jdbc","name":"lite","configuration":{"url":"jdbc:sqlite:/tmp/a.db"}}}}"#;
        let v = parse_dbeaver(j).unwrap();
        let pg = v.iter().find(|p| p.name == "Main PG").unwrap();
        assert_eq!((pg.host.as_str(), pg.port, pg.username.as_str(), pg.color.as_str()), ("pg1", 5433, "alice", "#dc2626"));
        let ora = v.iter().find(|p| p.name == "Ora").unwrap();
        assert_eq!((ora.oracle.service_type.as_str(), ora.oracle.connect_as.as_str()), ("sid", "sysdba"));
        let lite = v.iter().find(|p| p.name == "lite").unwrap();
        assert_eq!((lite.engine, lite.database.as_str()), (Engine::Sqlite, "/tmp/a.db"));
    }

    #[test]
    fn datagrip_xml() {
        let xml = r#"<project version="4"><component name="DataSourceManagerImpl">
          <data-source source="LOCAL" name="pg@localhost" uuid="u1" group="Dev"><driver-ref>postgresql</driver-ref>
            <jdbc-url>jdbc:postgresql://localhost:5432/postgres</jdbc-url></data-source>
          <data-source source="LOCAL" name="ms" uuid="u2"><jdbc-url>jdbc:sqlserver://s:1433;databaseName=db</jdbc-url></data-source>
        </component></project>"#;
        let local = r#"<project><component><data-source name="pg@localhost" uuid="u1"><user-name>postgres</user-name></data-source></component></project>"#;
        let v = parse_datagrip(xml, local).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].name.as_str(), v[0].username.as_str(), v[0].folder.as_str()), ("pg@localhost", "postgres", "Dev"));
        assert_eq!(v[1].engine, Engine::Mssql);
    }
}
