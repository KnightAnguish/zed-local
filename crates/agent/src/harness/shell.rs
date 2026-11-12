use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// Stable identifier for one background shell job.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShellJobId(String);

impl ShellJobId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ShellJobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Lifecycle of a background shell job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellJobStatus {
    Running,
    Completed {
        exit: Option<i32>,
    },
    Failed,
    Cancelled,
    /// The job was running when the thread was restored, so it cannot be
    /// resumed and its process is gone.
    Interrupted,
}

impl ShellJobStatus {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        !self.is_running()
    }
}

/// Bookkeeping for a background shell job.
///
/// Phase 0 only stores metadata. The terminal handle and completion callback
/// are added when the `terminal` tool gains background mode.
#[derive(Clone, Debug)]
pub struct ShellJob {
    pub id: ShellJobId,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub output_path: PathBuf,
    pub status: ShellJobStatus,
    pub started_at: Instant,
    pub timeout: Option<Duration>,
}

impl ShellJob {
    pub fn new(
        id: ShellJobId,
        command: impl Into<String>,
        cwd: Option<PathBuf>,
        output_path: PathBuf,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            id,
            command: command.into(),
            cwd,
            output_path,
            status: ShellJobStatus::Running,
            started_at: Instant::now(),
            timeout,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// Thread-scoped registry of background shell jobs.
#[derive(Clone, Debug, Default)]
pub struct ShellJobRegistry {
    jobs: HashMap<ShellJobId, ShellJob>,
}

impl ShellJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, job: ShellJob) -> Option<ShellJob> {
        self.jobs.insert(job.id.clone(), job)
    }

    pub fn get(&self, id: &ShellJobId) -> Option<&ShellJob> {
        self.jobs.get(id)
    }

    pub fn get_mut(&mut self, id: &ShellJobId) -> Option<&mut ShellJob> {
        self.jobs.get_mut(id)
    }

    pub fn remove(&mut self, id: &ShellJobId) -> Option<ShellJob> {
        self.jobs.remove(id)
    }

    pub fn list(&self) -> impl Iterator<Item = &ShellJob> {
        self.jobs.values()
    }

    pub fn running_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| job.status.is_running())
            .count()
    }

    pub fn can_start(&self, max_jobs: usize) -> bool {
        self.running_count() < max_jobs
    }

    pub fn set_status(&mut self, id: &ShellJobId, status: ShellJobStatus) -> bool {
        let Some(job) = self.jobs.get_mut(id) else {
            return false;
        };
        job.status = status;
        true
    }

    /// Marks every still-running job as interrupted. Called when a thread is
    /// restored; processes do not survive a restart.
    pub fn mark_running_interrupted(&mut self) {
        for job in self.jobs.values_mut() {
            if job.status.is_running() {
                job.status = ShellJobStatus::Interrupted;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str) -> ShellJob {
        ShellJob::new(
            ShellJobId::new(id),
            "echo hello",
            None,
            PathBuf::from("C:\\temp\\job.log"),
            Some(Duration::from_secs(30)),
        )
    }

    #[test]
    fn job_starts_running_and_becomes_terminal() {
        let job = job("one");
        assert!(job.status.is_running());
        assert!(!job.status.is_terminal());
        assert_eq!(job.id.as_str(), "one");
    }

    #[test]
    fn registry_tracks_running_jobs_and_limits() {
        let mut registry = ShellJobRegistry::new();
        assert!(registry.can_start(1));
        registry.insert(job("one"));
        assert_eq!(registry.running_count(), 1);
        assert!(!registry.can_start(1));
        assert!(registry.can_start(2));

        assert!(registry.set_status(
            &ShellJobId::new("one"),
            ShellJobStatus::Completed { exit: Some(0) }
        ));
        assert_eq!(registry.running_count(), 0);
        assert!(registry.can_start(1));
    }

    #[test]
    fn registry_rejects_unknown_jobs() {
        let mut registry = ShellJobRegistry::new();
        assert!(!registry.set_status(&ShellJobId::new("missing"), ShellJobStatus::Failed));
        assert!(registry.get(&ShellJobId::new("missing")).is_none());
    }

    #[test]
    fn restoring_a_thread_interrupts_running_jobs() {
        let mut registry = ShellJobRegistry::new();
        registry.insert(job("one"));
        registry.insert(job("two"));
        registry.set_status(
            &ShellJobId::new("two"),
            ShellJobStatus::Completed { exit: Some(0) },
        );

        registry.mark_running_interrupted();

        assert_eq!(
            registry.get(&ShellJobId::new("one")).map(|job| job.status),
            Some(ShellJobStatus::Interrupted)
        );
        assert_eq!(
            registry.get(&ShellJobId::new("two")).map(|job| job.status),
            Some(ShellJobStatus::Completed { exit: Some(0) })
        );
    }

    #[test]
    fn removing_a_job_returns_it() {
        let mut registry = ShellJobRegistry::new();
        registry.insert(job("one"));
        assert_eq!(
            registry
                .remove(&ShellJobId::new("one"))
                .map(|job| job.id.as_str().to_owned()),
            Some("one".to_owned())
        );
        assert!(registry.is_empty());
    }
}
