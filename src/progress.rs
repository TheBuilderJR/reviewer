use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug)]
pub struct ProgressReporter {
    started_at: Instant,
    agent_total: AtomicU64,
    agent_started: AtomicU64,
    agent_finished: AtomicU64,
}

impl ProgressReporter {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            agent_total: AtomicU64::new(0),
            agent_started: AtomicU64::new(0),
            agent_finished: AtomicU64::new(0),
        }
    }

    pub fn info(&self, area: &'static str, message: impl AsRef<str>) {
        self.emit(area, "INFO", message.as_ref());
    }

    pub fn begin_step(
        self: &Arc<Self>,
        area: &'static str,
        label: impl Into<String>,
    ) -> StepHandle {
        let label = label.into();
        self.emit(area, "START", &label);
        StepHandle {
            reporter: self.clone(),
            area,
            label,
            started_at: Instant::now(),
            completed: false,
        }
    }

    pub fn set_agent_total(&self, total: usize) {
        self.agent_total.store(total as u64, Ordering::Relaxed);
        self.agent_started.store(0, Ordering::Relaxed);
        self.agent_finished.store(0, Ordering::Relaxed);
        self.emit(
            "agents",
            "PLAN",
            &format!("{total} provider invocations queued"),
        );
    }

    pub fn begin_agent(self: &Arc<Self>, label: impl Into<String>) -> AgentHandle {
        let label = label.into();
        let ordinal = self.agent_started.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.agent_total.load(Ordering::Relaxed);
        self.emit("agent", "START", &format!("[{ordinal}/{total}] {label}"));
        AgentHandle {
            reporter: self.clone(),
            label,
            started_at: Instant::now(),
            completed: false,
        }
    }

    fn emit(&self, area: &'static str, status: &str, message: &str) {
        eprintln!(
            "[{:>6.1}s] {:<7} {:<6} {}",
            self.started_at.elapsed().as_secs_f32(),
            area.to_ascii_uppercase(),
            status,
            message
        );
    }
}

pub struct StepHandle {
    reporter: Arc<ProgressReporter>,
    area: &'static str,
    label: String,
    started_at: Instant,
    completed: bool,
}

impl StepHandle {
    pub fn done(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        self.reporter.emit(
            self.area,
            "DONE",
            &format!(
                "{} ({:.1}s){}",
                self.label,
                self.started_at.elapsed().as_secs_f32(),
                render_detail(detail.as_ref())
            ),
        );
    }

    pub fn fail(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        self.reporter.emit(
            self.area,
            "FAIL",
            &format!(
                "{} ({:.1}s){}",
                self.label,
                self.started_at.elapsed().as_secs_f32(),
                render_detail(detail.as_ref())
            ),
        );
    }
}

impl Drop for StepHandle {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            self.reporter.emit(self.area, "ABORT", &self.label);
        }
    }
}

pub struct AgentHandle {
    reporter: Arc<ProgressReporter>,
    label: String,
    started_at: Instant,
    completed: bool,
}

impl AgentHandle {
    pub fn done(mut self) {
        self.completed = true;
        let finished = self.reporter.agent_finished.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.reporter.agent_total.load(Ordering::Relaxed);
        self.reporter.emit(
            "agent",
            "DONE",
            &format!(
                "[{finished}/{total}] {} ({:.1}s)",
                self.label,
                self.started_at.elapsed().as_secs_f32()
            ),
        );
    }

    pub fn fail(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        let finished = self.reporter.agent_finished.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.reporter.agent_total.load(Ordering::Relaxed);
        self.reporter.emit(
            "agent",
            "FAIL",
            &format!(
                "[{finished}/{total}] {} ({:.1}s){}",
                self.label,
                self.started_at.elapsed().as_secs_f32(),
                render_detail(detail.as_ref())
            ),
        );
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            self.reporter.emit("agent", "ABORT", &self.label);
        }
    }
}

fn render_detail(detail: &str) -> String {
    if detail.trim().is_empty() {
        String::new()
    } else {
        format!(" -> {}", detail.trim())
    }
}
