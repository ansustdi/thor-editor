//! Light "Session Browser" (Toad-style): server sessions, current SQL, waits, blocking, locks,
//! long operations, cancel / kill. Every query uses one engine's system views.

use crate::drivers::{lit, CollectSink, Dialect, Session};
use crate::model::Cell;
use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbSession {
    /// Kill handle: Oracle "sid,serial#", PostgreSQL pid, MySQL/MSSQL id, ClickHouse query_id.
    pub id: String,
    pub sid: String,
    pub user: String,
    pub status: String,
    pub machine: String,
    pub program: String,
    pub os_user: String,
    pub schema: String,
    pub sql_text: String,
    pub sql_id: String,
    pub wait: String,
    /// Seconds in the current call / query.
    pub seconds: Option<i64>,
    /// SID(s) of the session(s) blocking this one.
    pub blocked_by: String,
    pub logon_time: String,
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Grid {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub sql_full: String,
    pub locks: Grid,
    pub long_ops: Grid,
}

async fn rows(s: &dyn Session, sql: &str) -> Result<Vec<Vec<Cell>>> {
    let mut sink = CollectSink::new(0);
    s.run(sql, &mut sink).await?;
    Ok(sink.rows)
}

async fn grid(s: &dyn Session, sql: &str) -> Grid {
    let mut sink = CollectSink::new(500);
    match s.run(sql, &mut sink).await {
        Ok(_) => Grid {
            columns: sink.columns.iter().map(|c| c.name.clone()).collect(),
            rows: sink.rows.iter().map(|r| r.iter().map(|c| if c.is_null() { String::new() } else { c.to_text() }).collect()).collect(),
        },
        Err(e) => Grid { columns: vec!["error".into()], rows: vec![vec![format!("{e:#}")]] },
    }
}

fn t(c: Option<&Cell>) -> String {
    match c {
        Some(Cell::Null) | None => String::new(),
        Some(v) => v.to_text(),
    }
}

/// The engine's session list in the uniform column order of `DbSession`.
fn list_sql(d: Dialect) -> Option<&'static str> {
    Some(match d {
        Dialect::Oracle => {
            "select s.sid || ',' || s.serial#, to_char(s.sid), s.username, s.status, s.machine, s.program, s.osuser, s.schemaname,
                    (select substr(a.sql_text, 1, 1000) from v$sqlarea a where a.sql_id = s.sql_id), s.sql_id,
                    case when s.state = 'WAITING' then s.event || ' (' || s.seconds_in_wait || 's)' when s.status = 'ACTIVE' then 'ON CPU' end,
                    s.last_call_et, to_char(s.blocking_session), to_char(s.logon_time, 'YYYY-MM-DD HH24:MI:SS'),
                    case when s.sid = sys_context('USERENV', 'SID') then 1 else 0 end
             from v$session s where s.type = 'USER'
             order by decode(s.status, 'ACTIVE', 0, 1), s.last_call_et desc"
        }
        Dialect::Postgres => {
            "select pid::text, pid::text, usename, coalesce(state, ''), coalesce(client_addr::text, client_hostname, 'local'), application_name, '',
                    datname, query, '', coalesce(wait_event_type || ': ' || wait_event, ''),
                    extract(epoch from now() - coalesce(query_start, backend_start))::bigint,
                    array_to_string(pg_blocking_pids(pid), ','), to_char(backend_start, 'YYYY-MM-DD HH24:MI:SS'),
                    (pid = pg_backend_pid())::int
             from pg_stat_activity where backend_type = 'client backend'
             order by (state = 'active') desc, query_start nulls last"
        }
        Dialect::Mysql => {
            "select cast(id as char), cast(id as char), user, concat(command, if(state is null or state = '', '', concat(' · ', state))), host, '', '',
                    coalesce(db, ''), coalesce(info, ''), '', coalesce(state, ''), time, '', '', id = connection_id()
             from information_schema.processlist where command <> 'Daemon'
             order by (command = 'Query') desc, time desc"
        }
        Dialect::Mssql => {
            "select cast(s.session_id as varchar(10)), cast(s.session_id as varchar(10)), s.login_name, coalesce(r.status, s.status), s.host_name,
                    s.program_name, s.nt_user_name, db_name(s.database_id), coalesce(t.text, ''), '',
                    coalesce(r.wait_type + ' (' + cast(r.wait_time / 1000 as varchar(10)) + 's)', ''),
                    datediff(second, coalesce(r.start_time, s.last_request_start_time), getdate()),
                    coalesce(cast(nullif(r.blocking_session_id, 0) as varchar(10)), ''), convert(varchar(19), s.login_time, 120),
                    case when s.session_id = @@spid then 1 else 0 end
             from sys.dm_exec_sessions s
             left join sys.dm_exec_requests r on r.session_id = s.session_id
             outer apply sys.dm_exec_sql_text(r.sql_handle) t
             where s.is_user_process = 1
             order by case when r.session_id is null then 1 else 0 end, s.session_id"
        }
        Dialect::Clickhouse => {
            "select query_id, query_id, user, 'running', toString(address), client_name, os_user, current_database, query, '', '',
                    toInt64(elapsed), '', '', 0
             from system.processes where query not like '%system.processes%' order by elapsed desc"
        }
        _ => return None,
    })
}

pub async fn list(s: &dyn Session) -> Result<Vec<DbSession>> {
    let Some(sql) = list_sql(s.dialect()) else { bail!("Session browser is not available for this database") };
    let r = rows(s, sql).await.map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("ORA-00942") {
            anyhow::anyhow!("Your Oracle user cannot read V$SESSION. Ask the DBA for SELECT_CATALOG_ROLE (or SELECT on V_$SESSION / V_$SQLAREA).\n{msg}")
        } else {
            e
        }
    })?;
    Ok(r.iter()
        .map(|r| DbSession {
            id: t(r.first()),
            sid: t(r.get(1)),
            user: t(r.get(2)),
            status: t(r.get(3)),
            machine: t(r.get(4)),
            program: t(r.get(5)),
            os_user: t(r.get(6)),
            schema: t(r.get(7)),
            sql_text: t(r.get(8)),
            sql_id: t(r.get(9)),
            wait: t(r.get(10)),
            seconds: t(r.get(11)).parse::<f64>().ok().map(|v| v as i64),
            blocked_by: t(r.get(12)),
            logon_time: t(r.get(13)),
            is_self: matches!(t(r.get(14)).as_str(), "1" | "true" | "t"),
        })
        .collect())
}

fn oracle_sid_serial(id: &str) -> Result<(i64, i64)> {
    let (a, b) = id.split_once(',').context("expected 'sid,serial#'")?;
    Ok((a.trim().parse()?, b.trim().parse()?))
}

fn numeric(id: &str) -> Result<i64> {
    id.trim().parse::<i64>().context("invalid session id")
}

pub async fn detail(s: &dyn Session, id: &str, sql_id: &str) -> Result<SessionDetail> {
    Ok(match s.dialect() {
        Dialect::Oracle => {
            let (sid, serial) = oracle_sid_serial(id)?;
            let sql_full = if sql_id.is_empty() {
                String::new()
            } else {
                rows(s, &format!("select sql_fulltext from v$sqlarea where sql_id = {}", lit(sql_id)))
                    .await
                    .ok()
                    .and_then(|r| r.first().map(|x| t(x.first())))
                    .unwrap_or_default()
            };
            SessionDetail {
                sql_full,
                locks: grid(
                    s,
                    &format!(
                        "select o.owner || '.' || o.object_name as object, o.object_type as type,
                                decode(lo.locked_mode, 1, 'Null', 2, 'Row Share', 3, 'Row Exclusive', 4, 'Share', 5, 'Share Row Excl', 6, 'Exclusive', to_char(lo.locked_mode)) as mode_held
                         from v$locked_object lo join all_objects o on o.object_id = lo.object_id where lo.session_id = {sid}"
                    ),
                )
                .await,
                long_ops: grid(
                    s,
                    &format!(
                        "select opname as operation, target, sofar, totalwork, round(sofar / nullif(totalwork, 0) * 100, 1) as pct,
                                time_remaining as remaining_s, elapsed_seconds as elapsed_s
                         from v$session_longops where sid = {sid} and serial# = {serial} and sofar <> totalwork order by start_time desc"
                    ),
                )
                .await,
            }
        }
        Dialect::Postgres => {
            let pid = numeric(id)?;
            let sql_full = rows(s, &format!("select query from pg_stat_activity where pid = {pid}"))
                .await
                .ok()
                .and_then(|r| r.first().map(|x| t(x.first())))
                .unwrap_or_default();
            SessionDetail {
                sql_full,
                locks: grid(
                    s,
                    &format!(
                        "select coalesce(relation::regclass::text, locktype) as object, locktype as type, mode, granted
                         from pg_locks where pid = {pid} order by granted, relation"
                    ),
                )
                .await,
                long_ops: grid(
                    s,
                    &format!(
                        "select 'VACUUM' as operation, relid::regclass::text as target, heap_blks_scanned as sofar, heap_blks_total as totalwork,
                                round(heap_blks_scanned * 100.0 / nullif(heap_blks_total, 0), 1) as pct
                         from pg_stat_progress_vacuum where pid = {pid}
                         union all
                         select 'CREATE INDEX', relid::regclass::text, blocks_done, blocks_total, round(blocks_done * 100.0 / nullif(blocks_total, 0), 1)
                         from pg_stat_progress_create_index where pid = {pid}"
                    ),
                )
                .await,
            }
        }
        Dialect::Mysql => {
            let pid = numeric(id)?;
            let sql_full = rows(s, &format!("select info from information_schema.processlist where id = {pid}"))
                .await
                .ok()
                .and_then(|r| r.first().map(|x| t(x.first())))
                .unwrap_or_default();
            SessionDetail { sql_full, ..Default::default() }
        }
        Dialect::Mssql => {
            let spid = numeric(id)?;
            let sql_full = rows(
                s,
                &format!("select t.text from sys.dm_exec_requests r cross apply sys.dm_exec_sql_text(r.sql_handle) t where r.session_id = {spid}"),
            )
            .await
            .ok()
            .and_then(|r| r.first().map(|x| t(x.first())))
            .unwrap_or_default();
            SessionDetail {
                sql_full,
                locks: grid(
                    s,
                    &format!(
                        "select coalesce(object_name(p.object_id, l.resource_database_id), l.resource_type) as object, l.resource_type as type,
                                l.request_mode as mode, l.request_status as status
                         from sys.dm_tran_locks l left join sys.partitions p on p.hobt_id = l.resource_associated_entity_id
                         where l.request_session_id = {spid}"
                    ),
                )
                .await,
                long_ops: grid(
                    s,
                    &format!("select command as operation, percent_complete as pct, estimated_completion_time / 1000 as remaining_s from sys.dm_exec_requests where session_id = {spid} and percent_complete > 0"),
                )
                .await,
            }
        }
        Dialect::Clickhouse => {
            let sql_full = rows(s, &format!("select query from system.processes where query_id = {}", lit(id)))
                .await
                .ok()
                .and_then(|r| r.first().map(|x| t(x.first())))
                .unwrap_or_default();
            SessionDetail {
                sql_full,
                long_ops: grid(
                    s,
                    &format!(
                        "select 'query' as operation, read_rows as sofar, total_rows_approx as totalwork,
                                round(read_rows * 100 / nullif(total_rows_approx, 0), 1) as pct, toInt64(elapsed) as elapsed_s
                         from system.processes where query_id = {}",
                        lit(id)
                    ),
                )
                .await,
                ..Default::default()
            }
        }
        _ => bail!("Session browser is not available for this database"),
    })
}

/// `cancel_only` stops the running statement but keeps the session; otherwise the session is killed.
pub async fn kill(s: &dyn Session, id: &str, cancel_only: bool) -> Result<()> {
    let sql = match s.dialect() {
        Dialect::Oracle => {
            let (sid, serial) = oracle_sid_serial(id)?;
            if cancel_only {
                format!("alter system cancel sql '{sid},{serial}'")
            } else {
                format!("alter system kill session '{sid},{serial}' immediate")
            }
        }
        Dialect::Postgres => {
            let pid = numeric(id)?;
            format!("select {}({pid})", if cancel_only { "pg_cancel_backend" } else { "pg_terminate_backend" })
        }
        Dialect::Mysql => format!("KILL {}{}", if cancel_only { "QUERY " } else { "" }, numeric(id)?),
        Dialect::Mssql => format!("KILL {}", numeric(id)?),
        Dialect::Clickhouse => format!("KILL QUERY WHERE query_id = {}", lit(id)),
        _ => bail!("not supported"),
    };
    s.exec(&sql).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(oracle_sid_serial("123, 4567").unwrap(), (123, 4567));
        assert!(oracle_sid_serial("123").is_err());
        assert!(numeric("12; drop").is_err());
    }
}
