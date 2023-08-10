use std::{rc::Rc, str::FromStr, time::Duration};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::Span;

use crate::{
    db_writer::{
        recurring::{AddRecurringJobArgs, DeleteRecurringJobArgs},
        DbOperation, UpsertMode,
    },
    shared_state::SharedState,
    Error, Job, JobBuilder, JobStatus, Queue,
};

pub(crate) async fn schedule_needed_recurring_jobs_at_startup(
    queue: &SharedState,
) -> Result<(), Error> {
    let conn = queue.read_conn_pool.get().await?;

    let needed_jobs = conn
        .interact(move |db| {
            let query = r##"SELECT base_job_id
                FROM recurring
                JOIN jobs on recurring.base_job_id = jobs.job_id
                LEFT JOIN active_jobs ON recurring.base_job_id = active_jobs.from_recurring
                WHERE active_jobs.job_id IS NULL"##;
            let mut stmt = db.prepare(query)?;
            let rows = stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;

            Ok::<_, Error>(rows)
        })
        .await??;
    drop(conn);

    schedule_recurring_jobs(queue, OffsetDateTime::now_utc(), &needed_jobs).await?;

    Ok(())
}

/// Schedule jobs from recurring templates.
pub(crate) async fn schedule_recurring_jobs(
    queue: &SharedState,
    from_time: OffsetDateTime,
    ids: &[i64],
) -> Result<(), Error> {
    let conn = queue.read_conn_pool.get().await?;
    let ids = ids
        .iter()
        .map(|id| rusqlite::types::Value::from(*id))
        .collect::<Vec<_>>();
    let jobs = conn
        .interact(move |db| {
            let query = r##"SELECT job_id,
                job_type, priority, weight, payload, max_retries,
                backoff_multiplier, backoff_randomization, backoff_initial_interval,
                default_timeout, heartbeat_increment, schedule
            FROM jobs
            JOIN recurring ON job_id = base_job_id
            WHERE status = 'recurring_base' AND job_id IN rarray(?)
            "##;

            let mut stmt = db.prepare_cached(query)?;

            let rows = stmt
                .query_and_then(params![Rc::new(ids)], |row| {
                    let job_id: i64 = row.get(0)?;
                    let job_type = row
                        .get_ref(1)?
                        .as_str()
                        .map(|s| s.to_string())
                        .map_err(|e| Error::ColumnType(e.into(), "job_type"))?;
                    let priority = row.get(2).map_err(|e| Error::ColumnType(e, "priority"))?;
                    let weight = row.get(3).map_err(|e| Error::ColumnType(e, "weight"))?;
                    let payload = row.get(4).map_err(|e| Error::ColumnType(e, "payload"))?;
                    let max_retries = row
                        .get(5)
                        .map_err(|e| Error::ColumnType(e, "max_retries"))?;
                    let backoff_multiplier = row
                        .get(6)
                        .map_err(|e| Error::ColumnType(e, "backoff_multiplier"))?;
                    let backoff_randomization = row
                        .get(7)
                        .map_err(|e| Error::ColumnType(e, "backoff_randomization"))?;
                    let backoff_initial_interval = row
                        .get(8)
                        .map_err(|e| Error::ColumnType(e, "backoff_initial_interval"))?;
                    let default_timeout = row
                        .get(9)
                        .map_err(|e| Error::ColumnType(e, "default_timeout"))?;
                    let heartbeat_increment = row
                        .get(10)
                        .map_err(|e| Error::ColumnType(e, "heartbeat_increment"))?;
                    let schedule = row
                        .get_ref(11)?
                        .as_str()
                        .map_err(|e| Error::ColumnType(e.into(), "schedule"))
                        .and_then(|s| {
                            serde_json::from_str::<RecurringJobSchedule>(s)
                                .map_err(|_| Error::InvalidSchedule)
                        })?;

                    let next_job_time = schedule.find_next_job_time(from_time)?;
                    let job = JobBuilder::new(job_type)
                        .priority(priority)
                        .weight(weight)
                        .payload(payload)
                        .max_retries(max_retries)
                        .backoff_multiplier(backoff_multiplier)
                        .backoff_randomization(backoff_randomization)
                        .backoff_initial_interval(Duration::from_secs(backoff_initial_interval))
                        .timeout(Duration::from_secs(default_timeout))
                        .heartbeat_increment(Duration::from_secs(heartbeat_increment))
                        .from_recurring(job_id)
                        .run_at(next_job_time)
                        .build();

                    Ok::<_, Error>(job)
                })?
                .collect::<Result<Vec<Job>, Error>>()?;

            Ok::<_, Error>(rows)
        })
        .await??;

    queue.add_jobs(jobs).await?;

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename = "snake_case")]
pub enum RecurringJobSchedule {
    Cron { spec: String },
    RepeatEvery { interval: Duration },
}

#[derive(Debug)]
pub struct RecurringJobInfo {
    pub base_job: JobStatus,
    pub schedule: RecurringJobSchedule,
    /// The status of the last (or current) run.
    pub last_run: Option<JobStatus>,
    /// The next time the job will run, if it's not currently running.
    pub next_run_at: Option<OffsetDateTime>,
}

impl RecurringJobSchedule {
    pub fn from_cron_string(spec: String) -> Result<Self, Error> {
        // Make sure it parses ok.
        cron::Schedule::from_str(&spec).map_err(|_| Error::InvalidSchedule)?;
        Ok(Self::Cron { spec })
    }

    pub(crate) fn find_next_job_time(
        &self,
        after: OffsetDateTime,
    ) -> Result<OffsetDateTime, Error> {
        match self {
            RecurringJobSchedule::Cron { spec } => {
                let sched = cron::Schedule::from_str(spec).map_err(|_| Error::InvalidSchedule)?;
                let after = chrono::DateTime::<chrono::Utc>::from_utc(
                    chrono::NaiveDateTime::from_timestamp_opt(after.unix_timestamp(), 0)
                        .ok_or(Error::TimestampOutOfRange("after"))?,
                    chrono::Utc,
                );
                let next = sched.after(&after).next().ok_or(Error::InvalidSchedule)?;
                // The `cron` package uses chrono but everything else here uses `time`, so convert.
                let next = OffsetDateTime::from_unix_timestamp(next.timestamp())
                    .map_err(|_| Error::InvalidSchedule)?;
                Ok(next)
            }
            RecurringJobSchedule::RepeatEvery { interval } => Ok(after + *interval),
        }
    }
}

impl Queue {
    /// Add a new recurring job to the queue, returning an error if a job with this external ID
    /// already exists.
    pub async fn add_recurring_job(
        &self,
        id: String,
        schedule: RecurringJobSchedule,
        job: Job,
        run_immediately: bool,
    ) -> Result<(), Error> {
        self.do_recurring_job_update(UpsertMode::Add, id, schedule, job, run_immediately)
            .await
    }

    /// Update a recurring job. Returns an error if the job does not exist.
    pub async fn update_recurring_job(
        &self,
        id: String,
        schedule: RecurringJobSchedule,
        job: Job,
    ) -> Result<(), Error> {
        self.do_recurring_job_update(UpsertMode::Update, id, schedule, job, false)
            .await
    }

    /// Add a new recurring job, or update an existing one.
    pub async fn upsert_recurring_job(
        &self,
        id: String,
        schedule: RecurringJobSchedule,
        job: Job,
        run_immediately_on_insert: bool,
    ) -> Result<(), Error> {
        self.do_recurring_job_update(
            UpsertMode::Upsert,
            id,
            schedule,
            job,
            run_immediately_on_insert,
        )
        .await
    }

    async fn do_recurring_job_update(
        &self,
        upsert_mode: UpsertMode,
        id: String,
        schedule: RecurringJobSchedule,
        job: Job,
        run_immediately_on_insert: bool,
    ) -> Result<(), Error> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let now = self.state.time.now();
        let job_type = job.job_type.to_string();
        self.state
            .db_write_tx
            .send(DbOperation {
                worker_id: 0,
                operation: crate::db_writer::DbOperationType::AddRecurringJob(
                    AddRecurringJobArgs {
                        external_id: id,
                        now,
                        schedule,
                        upsert_mode,
                        job,
                        result_tx,
                        run_immediately_on_insert,
                    },
                ),
                span: Span::current(),
            })
            .await
            .map_err(|_| Error::QueueClosed)?;
        let add_result = result_rx.await.map_err(|_| Error::QueueClosed)??;
        if let Some(run_at) = add_result.new_run_at {
            self.state.notify_for_job_type(now, run_at, &job_type).await;
        }

        Ok(())
    }

    /// Remove a recurring job and unschedule any scheduled jobs for it.
    pub async fn delete_recurring_job(&self, id: String) -> Result<(), Error> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.state
            .db_write_tx
            .send(DbOperation {
                worker_id: 0,
                span: Span::current(),
                operation: crate::db_writer::DbOperationType::DeleteRecurringJob(
                    DeleteRecurringJobArgs { id, result_tx },
                ),
            })
            .await
            .map_err(|_| Error::QueueClosed)?;
        result_rx.await.map_err(|_| Error::QueueClosed)??;
        Ok(())
    }

    /// Return information about a recurring job and its latest execution
    pub async fn get_recurring_job_info(&self, id: String) -> Result<RecurringJobInfo, Error> {
        let conn = self.state.read_conn_pool.get().await?;
        let recurring_info = conn
            .interact(move |db| {
                let mut base_info_stmt = db.prepare_cached(
                    r##"SELECT base_job_id, schedule
                FROM recurring
                WHERE external_id = ?"##,
                )?;
                let (base_job_id, schedule) = base_info_stmt.query_row(params![id], |row| {
                    let base_job_id = row.get(0)?;
                    let schedule = row.get::<_, String>(1)?;
                    Ok((base_job_id, schedule))
                })?;

                let schedule: RecurringJobSchedule =
                    serde_json::from_str(&schedule).map_err(|_| Error::InvalidSchedule)?;

                let base_job_info =
                    Self::run_job_status_query(db, crate::job_status::JobIdQuery::Id(base_job_id))?;

                let mut find_last_run_stmt = db.prepare_cached(
                    r##"SELECT job_id
                    FROM jobs
                    WHERE from_recurring_job = ? AND started_at IS NOT NULL
                    ORDER BY started_at DESC
                    LIMIT 1"##,
                )?;

                let last_run_id = find_last_run_stmt
                    .query_row([base_job_id], |row| row.get::<_, i64>(0))
                    .optional()?;

                let last_run = if let Some(last_run_id) = last_run_id {
                    Some(Self::run_job_status_query(
                        db,
                        crate::job_status::JobIdQuery::Id(last_run_id),
                    )?)
                } else {
                    None
                };

                let mut next_run_stmt = db.prepare_cached(
                    r##"SELECT orig_run_at
                    FROM jobs
                    WHERE from_recurring_job = ? AND started_at IS NULL
                    LIMIT 1"##,
                )?;

                let next_run_at = next_run_stmt
                    .query_row([base_job_id], |row| row.get::<_, i64>(0))
                    .optional()?
                    .map(OffsetDateTime::from_unix_timestamp)
                    .transpose()
                    .map_err(|_| Error::TimestampOutOfRange("orig_run_at"))?;

                Ok::<_, Error>(RecurringJobInfo {
                    base_job: base_job_info,
                    schedule,
                    last_run,
                    next_run_at,
                })
            })
            .await??;

        Ok(recurring_info)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{recurring::RecurringJobSchedule, test_util::TestEnvironment, JobBuilder};

    #[tokio::test]
    #[ignore]
    async fn simple_recurring() {
        let test = TestEnvironment::new().await;
        let _worker = test.worker().build().await.expect("Failed to build worker");
        let job = JobBuilder::new("counter")
            .json_payload(&serde_json::json!({ "value": 5 }))
            .expect("json_payload")
            .build();

        test.queue
            .add_recurring_job(
                "job_id".to_string(),
                RecurringJobSchedule::RepeatEvery {
                    interval: Duration::from_millis(10),
                },
                job,
                false,
            )
            .await
            .expect("add_recurring_job");
        let now = test.time.now();
    }

    #[tokio::test]
    #[ignore]
    async fn run_immediately() {}

    #[tokio::test]
    #[ignore]
    async fn failed_job_gets_rescheduled() {}

    #[tokio::test]
    #[ignore]
    async fn add_with_bad_schedule() {}

    #[tokio::test]
    #[ignore]
    async fn update_to_bad_schedule() {}

    #[tokio::test]
    #[ignore]
    async fn update_nonexisting_error() {}

    #[tokio::test]
    #[ignore]
    async fn add_already_existing_error() {}

    #[tokio::test]
    #[ignore]
    async fn update_recurring_same_schedule() {}

    #[tokio::test]
    #[ignore]
    async fn update_recurring_to_earlier() {}

    #[tokio::test]
    #[ignore]
    async fn update_recurring_to_later() {}

    #[tokio::test]
    #[ignore]
    async fn upsert_recurring() {}

    #[tokio::test]
    #[ignore]
    async fn delete_recurring() {}
}
