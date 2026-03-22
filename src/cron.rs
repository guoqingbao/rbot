use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use uuid::Uuid;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn modified_key(path: &Path) -> Option<u128> {
    let modified = path.metadata().ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_nanos(),
    )
}

fn compute_next_run(schedule: &CronSchedule, now: u64) -> Option<u64> {
    match schedule.kind {
        CronScheduleKind::At => schedule.at_ms.filter(|at| *at > now),
        CronScheduleKind::Every => schedule.every_ms.map(|every_ms| now + every_ms),
        CronScheduleKind::Cron => {
            let expr = schedule.expr.as_deref()?;
            let cron = Schedule::from_str(&normalize_cron_expr(expr)).ok()?;
            if let Some(tz_name) = &schedule.tz {
                let tz = Tz::from_str(tz_name).ok()?;
                let base = Utc
                    .timestamp_millis_opt(now as i64)
                    .single()?
                    .with_timezone(&tz);
                cron.after(&base)
                    .next()
                    .map(|dt| dt.with_timezone(&Utc).timestamp_millis() as u64)
            } else {
                let base: DateTime<Local> = Local.timestamp_millis_opt(now as i64).single()?;
                cron.after(&base)
                    .next()
                    .map(|dt| dt.with_timezone(&Utc).timestamp_millis() as u64)
            }
        }
    }
}

fn normalize_cron_expr(expr: &str) -> String {
    let fields = expr.split_whitespace().collect::<Vec<_>>();
    if fields.len() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

fn validate_schedule_for_add(schedule: &CronSchedule) -> Result<()> {
    if schedule.tz.is_some() && !matches!(schedule.kind, CronScheduleKind::Cron) {
        return Err(anyhow!("tz can only be used with cron schedules"));
    }
    if let Some(tz) = &schedule.tz {
        Tz::from_str(tz).map_err(|_| anyhow!("unknown timezone '{tz}'"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CronScheduleKind {
    At,
    Every,
    Cron,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct CronSchedule {
    pub kind: CronScheduleKind,
    pub at_ms: Option<u64>,
    pub every_ms: Option<u64>,
    pub expr: Option<String>,
    pub tz: Option<String>,
}

impl Default for CronSchedule {
    fn default() -> Self {
        Self {
            kind: CronScheduleKind::Every,
            at_ms: None,
            every_ms: None,
            expr: None,
            tz: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct CronPayload {
    pub kind: String,
    pub message: String,
    pub deliver: bool,
    pub channel: Option<String>,
    pub to: Option<String>,
}

impl Default for CronPayload {
    fn default() -> Self {
        Self {
            kind: "agent_turn".to_string(),
            message: String::new(),
            deliver: false,
            channel: None,
            to: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct CronRunRecord {
    pub run_at_ms: u64,
    pub status: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl Default for CronRunRecord {
    fn default() -> Self {
        Self {
            run_at_ms: 0,
            status: "ok".to_string(),
            duration_ms: 0,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct CronJobState {
    pub next_run_at_ms: Option<u64>,
    pub last_run_at_ms: Option<u64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub run_history: Vec<CronRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule: CronSchedule,
    pub payload: CronPayload,
    pub state: CronJobState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub delete_after_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct CronStore {
    pub version: u32,
    pub jobs: Vec<CronJob>,
}

type JobCallback = Arc<dyn Fn(CronJob) -> BoxFuture<'static, Result<()>> + Send + Sync>;

struct CronRuntime {
    store: CronStore,
    last_mtime: Option<u128>,
    running: bool,
    timer_task: Option<JoinHandle<()>>,
}

impl Default for CronRuntime {
    fn default() -> Self {
        Self {
            store: CronStore {
                version: 1,
                jobs: Vec::new(),
            },
            last_mtime: None,
            running: false,
            timer_task: None,
        }
    }
}

struct CronInner {
    store_path: PathBuf,
    on_job: Option<JobCallback>,
    state: Mutex<CronRuntime>,
}

#[derive(Clone)]
pub struct CronService {
    inner: Arc<CronInner>,
}

impl CronService {
    pub const MAX_RUN_HISTORY: usize = 20;

    pub fn new(store_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(CronInner {
                store_path: store_path.into(),
                on_job: None,
                state: Mutex::new(CronRuntime::default()),
            }),
        }
    }

    pub fn with_callback<F, Fut>(store_path: impl Into<PathBuf>, on_job: F) -> Self
    where
        F: Fn(CronJob) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        Self {
            inner: Arc::new(CronInner {
                store_path: store_path.into(),
                on_job: Some(Arc::new(move |job| Box::pin(on_job(job)))),
                state: Mutex::new(CronRuntime::default()),
            }),
        }
    }

    pub async fn start(&self) -> Result<()> {
        {
            let mut state = self.inner.state.lock().expect("cron state lock poisoned");
            self.refresh_store_locked(&mut state)?;
            state.running = true;
            let now = now_ms();
            for job in &mut state.store.jobs {
                if job.enabled {
                    job.state.next_run_at_ms = compute_next_run(&job.schedule, now);
                }
            }
            self.save_store_locked(&mut state)?;
        }
        self.arm_timer();
        Ok(())
    }

    pub fn stop(&self) {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        state.running = false;
        if let Some(task) = state.timer_task.take() {
            task.abort();
        }
    }

    pub fn list_jobs(&self, include_disabled: bool) -> Result<Vec<CronJob>> {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        let mut jobs = state.store.jobs.clone();
        if !include_disabled {
            jobs.retain(|job| job.enabled);
        }
        jobs.sort_by_key(|job| job.state.next_run_at_ms.unwrap_or(u64::MAX));
        Ok(jobs)
    }

    pub fn add_job(
        &self,
        name: &str,
        schedule: CronSchedule,
        message: &str,
        deliver: bool,
        channel: Option<String>,
        to: Option<String>,
        delete_after_run: bool,
    ) -> Result<CronJob> {
        validate_schedule_for_add(&schedule)?;
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        let now = now_ms();
        let job = CronJob {
            id: Uuid::new_v4().simple().to_string()[..8].to_string(),
            name: name.to_string(),
            enabled: true,
            schedule: schedule.clone(),
            payload: CronPayload {
                kind: "agent_turn".to_string(),
                message: message.to_string(),
                deliver,
                channel,
                to,
            },
            state: CronJobState {
                next_run_at_ms: compute_next_run(&schedule, now),
                ..CronJobState::default()
            },
            created_at_ms: now,
            updated_at_ms: now,
            delete_after_run,
        };
        state.store.jobs.push(job.clone());
        self.save_store_locked(&mut state)?;
        drop(state);
        self.arm_timer();
        Ok(job)
    }

    pub fn remove_job(&self, job_id: &str) -> Result<bool> {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        let before = state.store.jobs.len();
        state.store.jobs.retain(|job| job.id != job_id);
        let removed = state.store.jobs.len() != before;
        if removed {
            self.save_store_locked(&mut state)?;
        }
        drop(state);
        if removed {
            self.arm_timer();
        }
        Ok(removed)
    }

    pub fn enable_job(&self, job_id: &str, enabled: bool) -> Result<Option<CronJob>> {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        let now = now_ms();
        let mut updated = None;
        for job in &mut state.store.jobs {
            if job.id == job_id {
                job.enabled = enabled;
                job.updated_at_ms = now;
                job.state.next_run_at_ms = enabled
                    .then(|| compute_next_run(&job.schedule, now))
                    .flatten();
                updated = Some(job.clone());
                break;
            }
        }
        if updated.is_some() {
            self.save_store_locked(&mut state)?;
        }
        drop(state);
        if updated.is_some() {
            self.arm_timer();
        }
        Ok(updated)
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<CronJob>> {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        Ok(state
            .store
            .jobs
            .iter()
            .find(|job| job.id == job_id)
            .cloned())
    }

    pub async fn run_job(&self, job_id: &str, force: bool) -> Result<bool> {
        let job = {
            let mut state = self.inner.state.lock().expect("cron state lock poisoned");
            self.refresh_store_locked(&mut state)?;
            state
                .store
                .jobs
                .iter()
                .find(|job| job.id == job_id)
                .cloned()
        };
        let Some(job) = job else {
            return Ok(false);
        };
        if !force && !job.enabled {
            return Ok(false);
        }
        self.execute_job(job).await?;
        Ok(true)
    }

    pub fn status(&self) -> Result<(bool, usize, Option<u64>)> {
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        self.refresh_store_locked(&mut state)?;
        let next = state
            .store
            .jobs
            .iter()
            .filter(|job| job.enabled)
            .filter_map(|job| job.state.next_run_at_ms)
            .min();
        Ok((state.running, state.store.jobs.len(), next))
    }

    fn arm_timer(&self) {
        let next_wake = {
            let mut state = self.inner.state.lock().expect("cron state lock poisoned");
            if let Some(task) = state.timer_task.take() {
                task.abort();
            }
            if !state.running {
                return;
            }
            state
                .store
                .jobs
                .iter()
                .filter(|job| job.enabled)
                .filter_map(|job| job.state.next_run_at_ms)
                .min()
        };
        let Some(next_wake) = next_wake else {
            return;
        };
        let delay = next_wake.saturating_sub(now_ms());
        let this = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _ = this.on_timer().await;
        });
        let mut state = self.inner.state.lock().expect("cron state lock poisoned");
        state.timer_task = Some(handle);
    }

    async fn on_timer(&self) -> Result<()> {
        let due_jobs = {
            let mut state = self.inner.state.lock().expect("cron state lock poisoned");
            self.refresh_store_locked(&mut state)?;
            let now = now_ms();
            state
                .store
                .jobs
                .iter()
                .filter(|job| job.enabled)
                .filter(|job| job.state.next_run_at_ms.is_some_and(|next| now >= next))
                .cloned()
                .collect::<Vec<_>>()
        };
        for job in due_jobs {
            self.execute_job(job).await?;
        }
        self.arm_timer();
        Ok(())
    }

    async fn execute_job(&self, job: CronJob) -> Result<()> {
        let start = now_ms();
        let result = if let Some(callback) = &self.inner.on_job {
            callback(job.clone()).await
        } else {
            Ok(())
        };
        let end = now_ms();
        let status = if result.is_ok() { "ok" } else { "error" }.to_string();
        let error = result.err().map(|err| err.to_string());

        {
            let mut state = self.inner.state.lock().expect("cron state lock poisoned");
            self.refresh_store_locked(&mut state)?;
            if let Some(existing) = state.store.jobs.iter_mut().find(|item| item.id == job.id) {
                existing.state.last_run_at_ms = Some(start);
                existing.state.last_status = Some(status.clone());
                existing.state.last_error = error.clone();
                existing.updated_at_ms = end;
                existing.state.run_history.push(CronRunRecord {
                    run_at_ms: start,
                    status: status.clone(),
                    duration_ms: end.saturating_sub(start),
                    error: error.clone(),
                });
                if existing.state.run_history.len() > Self::MAX_RUN_HISTORY {
                    let keep_from = existing.state.run_history.len() - Self::MAX_RUN_HISTORY;
                    existing.state.run_history = existing.state.run_history[keep_from..].to_vec();
                }

                match existing.schedule.kind {
                    CronScheduleKind::At => {
                        if existing.delete_after_run {
                            state.store.jobs.retain(|item| item.id != job.id);
                        } else {
                            existing.enabled = false;
                            existing.state.next_run_at_ms = None;
                        }
                    }
                    _ => {
                        existing.state.next_run_at_ms =
                            compute_next_run(&existing.schedule, now_ms());
                    }
                }
            }
            self.save_store_locked(&mut state)?;
        }
        Ok(())
    }

    fn refresh_store_locked(&self, state: &mut CronRuntime) -> Result<()> {
        let current_mtime = modified_key(&self.inner.store_path);
        if current_mtime.is_none() {
            if state.store.version == 0 {
                state.store.version = 1;
            }
            return Ok(());
        }
        if state.last_mtime == current_mtime {
            return Ok(());
        }
        let raw = fs::read_to_string(&self.inner.store_path)?;
        let store: CronStore = serde_json::from_str(&raw)?;
        state.store = store;
        state.last_mtime = current_mtime;
        Ok(())
    }

    fn save_store_locked(&self, state: &mut CronRuntime) -> Result<()> {
        if let Some(parent) = self.inner.store_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if state.store.version == 0 {
            state.store.version = 1;
        }
        fs::write(
            &self.inner.store_path,
            serde_json::to_string_pretty(&state.store)?,
        )?;
        state.last_mtime = modified_key(&self.inner.store_path);
        Ok(())
    }
}
