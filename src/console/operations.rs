//! Structured lifecycle state, independent of log filtering and log strings.
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Waiting,
    Running,
    Succeeded,
    Failed,
    Skipped,
}
impl TaskState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Waiting => "WAITING",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Skipped => "SKIPPED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Task {
    pub name: &'static str,
    pub state: TaskState,
    pub detail: &'static str,
    pub changed: Instant,
    pub last_failure: Option<Instant>,
    pub failures: u64,
}
#[derive(Clone)]
pub struct Operations(Arc<Mutex<[Task; 2]>>);
impl Default for Operations {
    fn default() -> Self {
        let task = |name| Task {
            name,
            state: TaskState::Waiting,
            detail: "not run in this process",
            changed: Instant::now(),
            last_failure: None,
            failures: 0,
        };
        Self(Arc::new(Mutex::new([
            task("Automatic backup"),
            task("Account maintenance"),
        ])))
    }
}
impl Operations {
    pub fn set(&self, index: usize, state: TaskState, detail: &'static str) {
        if let Ok(mut tasks) = self.0.lock()
            && let Some(task) = tasks.get_mut(index)
        {
            task.state = state;
            task.detail = detail;
            task.changed = Instant::now();
            if state == TaskState::Failed {
                task.last_failure = Some(task.changed);
                task.failures += 1;
            }
        }
    }
    pub fn snapshot(&self) -> Vec<Task> {
        self.0
            .lock()
            .map_or_else(|_| Vec::new(), |tasks| tasks.to_vec())
    }
}

/// Persist only a diagnostic category, never arbitrary exception/user data.
pub fn failure_reason(error: &anyhow::Error) -> &'static str {
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<std::io::Error>() {
            return match error.kind() {
                std::io::ErrorKind::PermissionDenied => "permission denied",
                std::io::ErrorKind::NotFound => "required file/directory missing",
                std::io::ErrorKind::StorageFull => "storage full",
                std::io::ErrorKind::TimedOut => "operation timed out",
                _ => "filesystem/IO operation failed",
            };
        }
        if source.is::<rusqlite::Error>() {
            return "database operation failed (possibly busy/locked)";
        }
    }
    "operation failed; inspect configuration, permissions and event callsite"
}
