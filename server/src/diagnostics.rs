//! State captured while a stall is still in progress.
//!
//! Everything here answers a question that only has an answer *before* the restart: which
//! threads are blocked and in what, whether the db pool still has connections to give, and what
//! postgres thinks we are running. On 2026-07-31 the pipeline stopped for seven hours and none
//! of it was recorded; the restart that fixed the outage also erased the cause.
//!
//! Every probe is bounded and failure-tolerant. A dump runs *because* the process is unwell, so
//! it must not be able to make things worse, block, or panic.

use std::time::Duration;
use tracing::{info, warn};

/// How long the postgres probe may take in total. It runs on its own throwaway runtime with its
/// own connection, so it cannot inherit a wedged pool — but it can still be waiting on a server
/// that is itself the problem, hence the bound.
const DB_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Rows of `pg_stat_activity` to log. The oldest few are the interesting ones; a full dump of a
/// busy server would bury them.
const DB_PROBE_ROWS: i64 = 10;

/// Threads to list. Above any plausible thread count for this process (a handful of gasket
/// stages plus axum's pool), so the cap only ever fires on a runaway.
const MAX_THREADS_LOGGED: usize = 64;

/// Log everything that will be gone after a restart. `reason` names the trigger so a dump can be
/// told from the stall it describes.
pub fn dump(reason: &str, db_url: &str) {
    warn!(
        reason,
        progress = %crate::state::progress::summary(),
        rss_mb = crate::state::rss_mb(),
        fd_count = crate::state::fd_count(),
        "stall dump"
    );
    dump_threads();
    dump_pools();
    dump_db(db_url);
}

/// Every thread with its kernel state and the function it is blocked in.
///
/// `wchan` distinguishes the cases that look identical from outside: a thread in `ep_poll` is an
/// idle tokio runtime (a task suspended on an await parks the *task*, not the thread, so it
/// shows up here as an idle worker), while one in a socket read is blocked in a syscall — that
/// is a wedged connection, and it names the fd's owner. Threads are unnamed (gasket spawns
/// stages with a bare `std::thread::spawn`), so this cannot attribute a thread to a stage;
/// `progress::summary()` in the header line is what identifies the stalled hop.
fn dump_threads() {
    let threads = thread_lines();
    info!(count = threads.len(), threads = %threads.join(" "), "stall dump: threads");
}

/// One `tid:state:wchan:name` per thread. The name goes last because it is the only field that
/// can itself contain a colon, which keeps the line splittable.
fn thread_lines() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    entries
        .flatten()
        .take(MAX_THREADS_LOGGED)
        .filter_map(|entry| {
            let dir = entry.path();
            let tid = entry.file_name().to_string_lossy().to_string();
            // A thread can exit between the listing and these reads, leaving a directory whose
            // files are already gone. Drop it: a thread that finished is not blocked on
            // anything, and an all-empty line would just be noise in the dump.
            // `stat` field 3 is the state char, but field 2 (the executable name) is
            // parenthesised and may itself contain spaces — so parse after the last ')'.
            let state = read_trimmed(&dir.join("stat")).and_then(|s| {
                s.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_string))
            })?;
            let name = read_trimmed(&dir.join("comm")).unwrap_or_default();
            let wchan = read_trimmed(&dir.join("wchan")).unwrap_or_default();
            Some(format!("{tid}:{state}:{wchan}:{name}"))
        })
        .collect()
}

fn read_trimmed(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Connection accounting for every pool this process opened. All-checked-out with nothing idle
/// is the signature of queries that went out and never came back — the state in which every
/// db-touching request hangs while everything else keeps serving.
fn dump_pools() {
    for (i, (size, idle)) in crate::state::pool_stats().into_iter().enumerate() {
        info!(
            pool = i,
            connections = size,
            idle,
            in_use = size as usize - idle.min(size as usize),
            "stall dump: db pool"
        );
    }
}

/// The oldest non-idle queries on the server, which is the one artefact that identifies a hung
/// query while it is still hung. Runs on a throwaway current-thread runtime with a fresh
/// connection: the pools this process already has may be exactly what is wedged.
fn dump_db(db_url: &str) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let url = db_url.to_string();
    runtime.block_on(async move {
        match tokio::time::timeout(DB_PROBE_TIMEOUT, query_activity(&url)).await {
            Ok(Ok(rows)) => {
                for row in rows {
                    info!(activity = %row, "stall dump: pg_stat_activity");
                }
            }
            Ok(Err(e)) => warn!(error = %e, "stall dump: pg_stat_activity query failed"),
            Err(_) => warn!(
                timeout_s = DB_PROBE_TIMEOUT.as_secs(),
                "stall dump: pg_stat_activity timed out — the db itself is not answering"
            ),
        }
    });
}

async fn query_activity(db_url: &str) -> Result<Vec<String>, sqlx::Error> {
    use sqlx::Row;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(DB_PROBE_TIMEOUT)
        .connect(db_url)
        .await?;
    let rows = sqlx::query(
        r#"SELECT pid,
                  coalesce(application_name, '') AS app,
                  coalesce(state, '') AS state,
                  coalesce(wait_event_type, '') AS wait_type,
                  coalesce(wait_event, '') AS wait,
                  coalesce(extract(epoch FROM (now() - query_start))::bigint, -1) AS age_s,
                  left(regexp_replace(query, '\s+', ' ', 'g'), 160) AS query
           FROM pg_stat_activity
           WHERE state <> 'idle' AND pid <> pg_backend_pid()
           ORDER BY query_start NULLS LAST
           LIMIT $1"#,
    )
    .bind(DB_PROBE_ROWS)
    .fetch_all(&pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let pid: i32 = r.try_get("pid").unwrap_or(0);
            let app: String = r.try_get("app").unwrap_or_default();
            let state: String = r.try_get("state").unwrap_or_default();
            let wait_type: String = r.try_get("wait_type").unwrap_or_default();
            let wait: String = r.try_get("wait").unwrap_or_default();
            let age: i64 = r.try_get("age_s").unwrap_or(-1);
            let query: String = r.try_get("query").unwrap_or_default();
            format!(
                "pid={pid} app={app} state={state} wait={wait_type}/{wait} age={age}s q={query}"
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dump's value is entirely in what it captures, so assert on the shape rather than on
    /// it merely not panicking: a silently empty `wchan` or state would read as "nothing was
    /// blocked" in the one record we get from a stall.
    #[test]
    fn thread_lines_describe_the_running_threads() {
        let lines = thread_lines();
        assert!(!lines.is_empty(), "no threads listed for our own process");
        for line in &lines {
            // `splitn`, not `split`: a thread name may contain colons (Rust names test threads
            // after their module path), which is why it is the trailing field.
            let fields: Vec<&str> = line.splitn(4, ':').collect();
            assert_eq!(fields.len(), 4, "malformed thread line: {line}");
            assert!(
                fields[0].parse::<u32>().is_ok(),
                "first field is not a tid: {line}"
            );
            // A thread is always in some state; an empty one means the `stat` parse broke on
            // the parenthesised comm field.
            assert!(!fields[1].is_empty(), "no thread state parsed: {line}");
        }
    }

    /// The postgres probe end to end, against a real server (`cargo test` already needs one).
    /// `#[ignore]`d and printing rather than asserting: what it returns depends on what the
    /// server happens to be running, and the point is to see the output.
    ///
    ///     cargo test --  --nocapture --ignored pg_stat_activity_probe
    #[test]
    #[ignore]
    fn pg_stat_activity_probe() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let rows = runtime
            .block_on(query_activity(&url))
            .expect("probe failed");
        println!("{} non-idle backends", rows.len());
        for row in rows {
            println!("  {row}");
        }
    }
}
