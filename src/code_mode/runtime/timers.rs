use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordedTimerCall {
    pub timer_id: String,
    pub delay_ms: u64,
    pub due_at_ms: u64,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub cleared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimerRegistration {
    pub timer_id: String,
    pub run_immediately: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingTimerState {
    pub pending_timers: usize,
    pub next_timer_id: Option<String>,
    pub resume_after_ms: Option<u64>,
    pub due_timer_ids: Vec<String>,
}

impl PendingTimerState {
    pub fn has_pending_timers(&self) -> bool {
        self.pending_timers > 0
    }
}

pub fn register_timeout(
    timer_calls: &mut Vec<RecordedTimerCall>,
    delay_ms: u64,
    now_ms: u64,
) -> TimerRegistration {
    let timer_id = format!("timer_{}", timer_calls.len() + 1);
    let due_at_ms = now_ms.saturating_add(delay_ms);
    let run_immediately = delay_ms == 0;

    timer_calls.push(RecordedTimerCall {
        timer_id: timer_id.clone(),
        delay_ms,
        due_at_ms,
        completed: false,
        cleared: false,
    });

    TimerRegistration {
        timer_id,
        run_immediately,
    }
}

pub fn clear_timeout(timer_calls: &mut [RecordedTimerCall], timer_id: &str) {
    if let Some(timer) = timer_calls
        .iter_mut()
        .find(|timer| timer.timer_id == timer_id)
    {
        timer.cleared = true;
    }
}

pub fn mark_timeout_completed(timer_calls: &mut [RecordedTimerCall], timer_id: &str) {
    if let Some(timer) = timer_calls
        .iter_mut()
        .find(|timer| timer.timer_id == timer_id)
    {
        timer.completed = true;
    }
}

pub fn pending_timer_state(timer_calls: &[RecordedTimerCall], now_ms: u64) -> PendingTimerState {
    let mut pending_timers = 0usize;
    let mut next_timer_id = None;
    let mut resume_after_ms = None;
    let mut due_timer_ids = Vec::new();

    for timer in timer_calls.iter() {
        if timer.cleared || timer.completed {
            continue;
        }

        pending_timers += 1;
        let remaining_ms = timer.due_at_ms.saturating_sub(now_ms);
        if remaining_ms == 0 {
            due_timer_ids.push(timer.timer_id.clone());
        }
        let should_replace = resume_after_ms
            .map(|current| remaining_ms < current)
            .unwrap_or(true);
        if should_replace {
            next_timer_id = Some(timer.timer_id.clone());
            resume_after_ms = Some(remaining_ms);
        }
    }

    PendingTimerState {
        pending_timers,
        next_timer_id,
        resume_after_ms,
        due_timer_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_timeout_tracks_pending_state() {
        let mut calls = Vec::new();
        let first = register_timeout(&mut calls, 25, 1_000);
        assert_eq!(first.timer_id, "timer_1");
        assert!(!first.run_immediately);

        let pending = pending_timer_state(&calls, 1_010);
        assert!(pending.has_pending_timers());
        assert_eq!(pending.pending_timers, 1);
        assert_eq!(pending.next_timer_id.as_deref(), Some("timer_1"));
        assert_eq!(pending.resume_after_ms, Some(15));
        assert!(pending.due_timer_ids.is_empty());
    }

    #[test]
    fn clear_and_complete_remove_timers_from_pending_state() {
        let mut calls = Vec::new();
        let first = register_timeout(&mut calls, 0, 1_000);
        assert!(first.run_immediately);
        mark_timeout_completed(&mut calls, &first.timer_id);
        assert!(!pending_timer_state(&calls, 1_000).has_pending_timers());

        let second = register_timeout(&mut calls, 50, 1_000);
        assert!(!second.run_immediately);
        clear_timeout(&mut calls, &second.timer_id);
        assert!(!pending_timer_state(&calls, 1_010).has_pending_timers());
    }

    #[test]
    fn pending_timer_state_surfaces_due_timer_ids() {
        let mut calls = Vec::new();
        let delayed = register_timeout(&mut calls, 25, 1_000);
        let immediate = register_timeout(&mut calls, 0, 1_000);

        let pending = pending_timer_state(&calls, 1_025);
        assert_eq!(pending.pending_timers, 2);
        assert_eq!(
            pending.next_timer_id.as_deref(),
            Some(delayed.timer_id.as_str())
        );
        assert_eq!(pending.resume_after_ms, Some(0));
        assert_eq!(
            pending.due_timer_ids,
            vec![delayed.timer_id, immediate.timer_id]
        );
    }
}
