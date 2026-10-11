use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use anyhow::{bail, Result};
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub struct LiteSession {
    conn: Mutex<Connection>,
    interrupt: InterruptHandle,
    cancel: CancelFlag,
    manual_commit: AtomicBool,
}

impl LiteSession {
    pub async fn connect(p: &ConnectionProfile) -> Result<Self> {
        let path = p.database.trim();
        if path.is_empty() {
            bail!("SQLite database file path is required");
        }
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI;
        if p.params.get("create").map(|v| v == "true").unwrap_or(true) {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let interrupt = conn.get_interrupt_handle();
        Ok(Self {
            conn: Mutex::new(conn),
            interrupt,
            cancel: CancelFlag::default(),
            manual_commit: AtomicBool::new(false),
        })
    }

    fn run_blocking(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("connection poisoned"))?;
        if self.manual_commit.load(Ordering::SeqCst) && conn.is_autocommit() {
            conn.execute_batch("BEGIN")?;
        }
        let mut stmt = conn.prepare(sql)?;
        let n = stmt.column_count();
        if n == 0 {
            let changed = stmt.execute([])?;
            return Ok(RunOutcome { affected: Some(changed as u64) });
        }
        let metas = stmt
            .columns()
            .iter()
            .map(|c| {
                let t = c.decl_type().unwrap_or("").to_string();
                let up = t.to_ascii_uppercase();
                let num = ["INT", "REAL", "FLOA", "DOUB", "NUM", "DEC"].iter().any(|k| up.contains(k));
                ColumnMeta::new(c.name(), t, num)
            })
            .collect();
        sink.columns(metas)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                cells.push(match row.get_ref(i)? {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(v) => Cell::Int(v),
                    ValueRef::Real(v) => Cell::Float(v),
                    ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
                });
            }
            if !sink.row(cells)? || self.cancel.is_set() {
                break;
            }
        }
        Ok(RunOutcome { affected: None })
    }
}

#[async_trait]
impl Session for LiteSession {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        tokio::task::block_in_place(|| self.run_blocking(sql, sink))
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel.set();
        self.interrupt.interrupt();
        Ok(())
    }

    async fn set_autocommit(&self, on: bool) -> Result<()> {
        self.manual_commit.store(!on, Ordering::SeqCst);
        if on {
            self.commit().await?;
        }
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("connection poisoned"))?;
        if !conn.is_autocommit() {
            conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("connection poisoned"))?;
        if !conn.is_autocommit() {
            conn.execute_batch("ROLLBACK")?;
        }
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }
}
