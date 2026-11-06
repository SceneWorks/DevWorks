//! A query's work and memory budget covers every phase, including linking and ranking.
use crate::ports::storage::Cancellation;
use crate::protocol::data::QueryLimits;
use std::time::{Duration, Instant};

/// An engineering memory bound, reported as partial coverage when reached. A client can
/// continue a stable page; exhausting this bound never manufactures an exact total.
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct Budget {
    started: Instant,
    deadline: Duration,
    max_visited: u32,
    pub visited: u32,
    retained_bytes: usize,
    pub exhausted: Option<&'static str>,
    cancellation: Option<Cancellation>,
}

impl Budget {
    pub fn new(limits: &QueryLimits) -> Self {
        Self {
            started: Instant::now(),
            deadline: Duration::from_millis(limits.deadline_ms.into()),
            max_visited: limits.max_visited,
            visited: 0,
            retained_bytes: 0,
            exhausted: None,
            cancellation: None,
        }
    }

    pub fn visit(&mut self) -> bool {
        if self.exhausted.is_some() {
            return false;
        }
        if self
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            self.exhausted = Some("graph_cancelled");
            return false;
        }
        if self.started.elapsed() >= self.deadline {
            self.exhausted = Some("graph_deadline");
            return false;
        }
        if self.visited >= self.max_visited {
            self.exhausted = Some("graph_visited_limit");
            return false;
        }
        self.visited += 1;
        true
    }

    pub fn start_at(&mut self, started: Instant, cancellation: Option<Cancellation>) {
        self.started = started;
        self.cancellation = cancellation;
    }

    /// Carry prior query phases into this budget; neither work nor retained memory resets.
    pub fn absorb_usage(&mut self, prior: &Self) {
        self.started = self.started.min(prior.started);
        self.visited = self.visited.saturating_add(prior.visited);
        self.exhausted = self.exhausted.or(prior.exhausted);
        self.retain(prior.retained_bytes);
        if self.visited > self.max_visited {
            self.exhausted = Some("graph_visited_limit");
        }
    }

    pub fn retain(&mut self, bytes: usize) -> bool {
        match self.retained_bytes.checked_add(bytes) {
            Some(total) if total <= MAX_RETAINED_BYTES => {
                self.retained_bytes = total;
                true
            }
            _ => {
                self.exhausted = Some("graph_memory_limit");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_budget_accounts_for_all_phases_and_reports_the_actual_limit() {
        let mut budget = Budget::new(&QueryLimits {
            max_results: 1,
            max_visited: 2,
            deadline_ms: 300_000,
        });
        assert!(budget.visit());
        assert!(budget.visit());
        assert!(!budget.visit());
        assert_eq!(budget.exhausted, Some("graph_visited_limit"));
        assert_eq!(budget.visited, 2);
        let mut memory = Budget::new(&QueryLimits {
            max_results: 1,
            max_visited: 2,
            deadline_ms: 300_000,
        });
        assert!(memory.retain(MAX_RETAINED_BYTES));
        assert!(!memory.retain(1));
        assert_eq!(memory.exhausted, Some("graph_memory_limit"));
    }
}
