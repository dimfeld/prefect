use rusqlite::{named_params, params, Connection, Transaction};
use tokio::sync::oneshot;

use crate::{Error, Job, Result};

pub(crate) struct CompleteJobArgs {
    pub job_id: i64,
    pub run_info: String,
    pub now: i64,
    pub started_at: i64,
    pub success: bool,
    pub result_tx: oneshot::Sender<Result<()>>,
}

fn do_complete_job(
    tx: &Connection,
    job_id: i64,
    worker_id: u64,
    now: i64,
    started_at: i64,
    success: bool,
    this_run_info: String,
) -> Result<()> {
    let mut delete_stmt =
        tx.prepare_cached(r##"DELETE FROM active_jobs WHERE job_id=?1 AND active_worker_id=?2"##)?;

    let altered = delete_stmt.execute(params![job_id, worker_id])?;
    if altered == 0 {
        return Err(Error::Expired);
    }

    let mut stmt = tx.prepare_cached(
        r##"
        UPDATE jobs SET
            status = $status,
            run_info = json_array_append(run_info, $this_run_info),
            started_at = $started_at,
            finished_at = $now
        WHERE job_id=$job_id
        "##,
    )?;

    stmt.execute(named_params! {
        "$job_id": job_id,
        "$now": now,
        "$started_at": started_at,
        "$this_run_info": this_run_info,
        "$status": if success { "success" } else { "failed" },
    })?;

    Ok(())
}

pub(crate) fn complete_job(tx: &Connection, worker_id: u64, args: CompleteJobArgs) -> bool {
    let CompleteJobArgs {
        job_id,
        run_info,
        now,
        started_at,
        success,
        result_tx,
    } = args;

    let result = do_complete_job(tx, job_id, worker_id, now, started_at, success, run_info);
    let worked = result.is_ok();
    result_tx.send(result).ok();
    worked
}