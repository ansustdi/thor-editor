use super::{is_dml, CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type MsClient = Client<Compat<TcpStream>>;

pub struct MsSession {
    client: Mutex<MsClient>,
    cancel: CancelFlag,
}

impl MsSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let mut cfg = Config::new();
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        cfg.host(host);
        cfg.port(p.port_or_default());
        if !p.database.is_empty() {
            cfg.database(&p.database);
        }
        cfg.application_name("Thor Editor");
        match p.ssl_mode.as_str() {
            "disable" => cfg.encryption(EncryptionLevel::NotSupported),
            "require" => {
                cfg.encryption(EncryptionLevel::Required);
                cfg.trust_cert();
            }
            "verify-full" => cfg.encryption(EncryptionLevel::Required),
            _ => {
                cfg.encryption(EncryptionLevel::Off);
                cfg.trust_cert();
            }
        }
        #[cfg(windows)]
        if p.auth == "windows" {
            cfg.authentication(AuthMethod::Integrated);
        } else {
            cfg.authentication(AuthMethod::sql_server(&p.username, password));
        }
        #[cfg(not(windows))]
        cfg.authentication(AuthMethod::sql_server(&p.username, password));

        let tcp = TcpStream::connect(cfg.get_addr()).await.context("SQL Server TCP connect failed")?;
        tcp.set_nodelay(true)?;
        let client = Client::connect(cfg, tcp.compat_write()).await.context("SQL Server login failed")?;
        Ok(Self { client: Mutex::new(client), cancel: CancelFlag::default() })
    }
}

fn numeric(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(t, Int1 | Int2 | Int4 | Int8 | Intn | Float4 | Float8 | Floatn | Decimaln | Numericn | Money | Money4)
}

fn to_cell(d: ColumnData<'static>) -> Cell {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    let opt = |o: Option<Cell>| o.unwrap_or(Cell::Null);
    match &d {
        ColumnData::U8(v) => opt(v.map(|x| Cell::Int(x as i64))),
        ColumnData::I16(v) => opt(v.map(|x| Cell::Int(x as i64))),
        ColumnData::I32(v) => opt(v.map(|x| Cell::Int(x as i64))),
        ColumnData::I64(v) => opt(v.map(Cell::Int)),
        ColumnData::F32(v) => opt(v.map(|x| Cell::Float(x as f64))),
        ColumnData::F64(v) => opt(v.map(Cell::Float)),
        ColumnData::Bit(v) => opt(v.map(Cell::Bool)),
        ColumnData::String(v) => opt(v.as_ref().map(|s| Cell::Text(s.to_string()))),
        ColumnData::Guid(v) => opt(v.map(|g| Cell::Text(g.to_string().to_uppercase()))),
        ColumnData::Binary(v) => opt(v.as_ref().map(|b| Cell::Bytes(b.to_vec()))),
        ColumnData::Numeric(v) => opt(v.map(|n| Cell::Dec(n.to_string()))),
        ColumnData::Xml(v) => opt(v.as_ref().map(|x| Cell::Text(x.to_string()))),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            opt(NaiveDateTime::from_sql(&d).ok().flatten().map(|t| Cell::DateTime(t.to_string())))
        }
        ColumnData::Date(_) => opt(NaiveDate::from_sql(&d).ok().flatten().map(|t| Cell::DateTime(t.to_string()))),
        ColumnData::Time(_) => opt(NaiveTime::from_sql(&d).ok().flatten().map(|t| Cell::DateTime(t.to_string()))),
        ColumnData::DateTimeOffset(_) => opt(
            DateTime::<FixedOffset>::from_sql(&d).ok().flatten().map(|t| Cell::DateTime(t.to_rfc3339())),
        ),
    }
}

#[async_trait]
impl Session for MsSession {
    fn dialect(&self) -> Dialect {
        Dialect::Mssql
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let mut client = self.client.lock().await;
        if is_dml(sql) && !sql.to_ascii_lowercase().contains("output") {
            let res = client.execute(sql, &[]).await?;
            return Ok(RunOutcome { affected: Some(res.total()) });
        }
        let mut stream = client.simple_query(sql).await?;
        let mut seen_meta = false;
        while let Some(item) = stream.try_next().await? {
            match item {
                QueryItem::Metadata(meta) => {
                    if seen_meta {
                        break; // only the first result set is shown
                    }
                    seen_meta = true;
                    sink.columns(
                        meta.columns()
                            .iter()
                            .map(|c| {
                                let t = c.column_type();
                                ColumnMeta::new(c.name(), format!("{t:?}").to_ascii_lowercase(), numeric(t))
                            })
                            .collect(),
                    )?;
                }
                QueryItem::Row(row) => {
                    let cells = row.into_iter().map(to_cell).collect();
                    if !sink.row(cells)? || self.cancel.is_set() {
                        break;
                    }
                }
            }
        }
        Ok(RunOutcome { affected: if seen_meta { None } else { Some(0) } })
    }

    async fn set_autocommit(&self, on: bool) -> Result<()> {
        self.exec(if on { "SET IMPLICIT_TRANSACTIONS OFF" } else { "SET IMPLICIT_TRANSACTIONS ON" }).await
    }
    async fn commit(&self) -> Result<()> {
        self.exec("IF @@TRANCOUNT > 0 COMMIT").await
    }
    async fn rollback(&self) -> Result<()> {
        self.exec("IF @@TRANCOUNT > 0 ROLLBACK").await
    }

    async fn ping(&self) -> Result<()> {
        let Ok(mut client) = self.client.try_lock() else { return Ok(()) };
        client.simple_query("SELECT 1").await?.into_results().await?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let mut client = self.client.lock().await;
        let row = client.simple_query("SELECT @@VERSION").await?.into_row().await?;
        Ok(row.and_then(|r| r.get::<&str, _>(0).map(str::to_string)).unwrap_or_default())
    }
}
