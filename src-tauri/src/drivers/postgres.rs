use super::{leading_keyword, CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{pin_mut, TryStreamExt};
use postgres_native_tls::MakeTlsConnector;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, NoTls, SimpleQueryMessage};

pub struct PgSession {
    client: Client,
    cancel_token: CancelToken,
    tls: Option<MakeTlsConnector>,
    cancel: CancelFlag,
    manual_commit: AtomicBool,
    in_tx: AtomicBool,
}

impl PgSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(if p.host.is_empty() { "localhost" } else { &p.host })
            .port(p.port_or_default())
            .user(&p.username)
            .password(password)
            .application_name("Thor Editor")
            .connect_timeout(std::time::Duration::from_secs(15));
        if !p.database.is_empty() {
            cfg.dbname(&p.database);
        }
        let (client, tls) = match p.ssl_mode.as_str() {
            "" | "disable" => {
                cfg.ssl_mode(SslMode::Disable);
                let (client, conn) = cfg.connect(NoTls).await.context("PostgreSQL connect failed")?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                (client, None)
            }
            mode => {
                cfg.ssl_mode(if mode == "prefer" { SslMode::Prefer } else { SslMode::Require });
                let verify = mode == "verify-full";
                let connector = native_tls::TlsConnector::builder()
                    .danger_accept_invalid_certs(!verify)
                    .danger_accept_invalid_hostnames(!verify)
                    .build()?;
                let tls = MakeTlsConnector::new(connector);
                let (client, conn) =
                    cfg.connect(tls.clone()).await.context("PostgreSQL connect failed")?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                (client, Some(tls))
            }
        };
        let cancel_token = client.cancel_token();
        Ok(Self {
            client,
            cancel_token,
            tls,
            cancel: CancelFlag::default(),
            manual_commit: AtomicBool::new(false),
            in_tx: AtomicBool::new(false),
        })
    }

    async fn column_types(&self, sql: &str) -> Option<Vec<Type>> {
        let kw = leading_keyword(sql);
        if !matches!(kw.as_str(), "select" | "with" | "values" | "table" | "show" | "explain") {
            return None;
        }
        let stmt = self.client.prepare(sql).await.ok()?;
        Some(stmt.columns().iter().map(|c| c.type_().clone()).collect())
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(
        *t,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID | Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC | Type::MONEY
    )
}

fn to_cell(v: Option<&str>, t: Option<&Type>) -> Cell {
    let Some(s) = v else { return Cell::Null };
    let Some(t) = t else { return Cell::Text(s.to_string()) };
    match *t {
        Type::BOOL => Cell::Bool(s == "t"),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => {
            s.parse().map(Cell::Int).unwrap_or_else(|_| Cell::Dec(s.into()))
        }
        Type::FLOAT4 | Type::FLOAT8 => s.parse().map(Cell::Float).unwrap_or_else(|_| Cell::Dec(s.into())),
        Type::NUMERIC => Cell::Dec(s.into()),
        Type::BYTEA => s
            .strip_prefix("\\x")
            .and_then(|h| hex::decode(h).ok())
            .map(Cell::Bytes)
            .unwrap_or_else(|| Cell::Text(s.into())),
        Type::DATE | Type::TIME | Type::TIMETZ | Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            Cell::DateTime(s.into())
        }
        _ => Cell::Text(s.into()),
    }
}

#[async_trait]
impl Session for PgSession {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        if self.manual_commit.load(Ordering::SeqCst) && !self.in_tx.load(Ordering::SeqCst) {
            self.client.batch_execute("BEGIN").await?;
            self.in_tx.store(true, Ordering::SeqCst);
        }
        let types = self.column_types(sql).await;
        let stream = self.client.simple_query_raw(sql).await?;
        pin_mut!(stream);

        let mut out = RunOutcome::default();
        let mut described = false;
        let mut done_first = false;
        while let Some(msg) = stream.try_next().await? {
            match msg {
                SimpleQueryMessage::RowDescription(cols) if !done_first => {
                    described = true;
                    let metas = cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let t = types.as_ref().and_then(|t| t.get(i));
                            ColumnMeta::new(
                                c.name(),
                                t.map(|t| t.name().to_string()).unwrap_or_default(),
                                t.map(is_numeric).unwrap_or(false),
                            )
                        })
                        .collect();
                    sink.columns(metas)?;
                }
                SimpleQueryMessage::Row(row) if !done_first => {
                    let cells = (0..row.len())
                        .map(|i| to_cell(row.get(i), types.as_ref().and_then(|t| t.get(i))))
                        .collect();
                    if !sink.row(cells)? || self.cancel.is_set() {
                        break;
                    }
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    if !described && !done_first {
                        out.affected = Some(n);
                    }
                    done_first = true;
                }
                _ => {}
            }
        }
        if matches!(leading_keyword(sql).as_str(), "commit" | "rollback" | "end" | "abort") {
            self.in_tx.store(false, Ordering::SeqCst);
        }
        Ok(out)
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel.set();
        match &self.tls {
            Some(tls) => self.cancel_token.cancel_query(tls.clone()).await?,
            None => self.cancel_token.cancel_query(NoTls).await?,
        }
        Ok(())
    }

    async fn set_autocommit(&self, on: bool) -> Result<()> {
        if on && self.in_tx.swap(false, Ordering::SeqCst) {
            self.client.batch_execute("COMMIT").await?;
        }
        self.manual_commit.store(!on, Ordering::SeqCst);
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        if self.in_tx.swap(false, Ordering::SeqCst) {
            self.client.batch_execute("COMMIT").await?;
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        if self.in_tx.swap(false, Ordering::SeqCst) {
            self.client.batch_execute("ROLLBACK").await?;
        }
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        self.client.simple_query("SELECT 1").await?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let row = self.client.query_one("select version()", &[]).await?;
        Ok(row.get::<_, String>(0))
    }
}
