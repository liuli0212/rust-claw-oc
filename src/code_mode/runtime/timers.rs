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
#[serde(rename_all = "snake_case")]
pub enum TimerAction {
    Run,
    Pending,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimerRegistration {
    pub timer_id: String,
    pub action: TimerAction,
    pub remaining_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingTimerState {
    pub pending_timers: usize,
    pub next_timer_id: Option<String>,
    pub resume_after_ms: Option<u64>,
    pub completed_ids: Vec<String>,
}

impl PendingTimerState {
    pub fn has_pending_timers(&self) -> bool {
        self.pending_timers > 0
    }
}

pub fn register_timeout(
    timer_calls: &mut Vec<RecordedTimerCall>,
    call_index: usize,
    delay_ms: u64,
    now_ms: u64,
) -> Result<TimerRegistration, String> {
    if let Some(recorded) = timer_calls.get(call_index) {
        if recorded.delay_ms != delay_ms {
            return Err(format!(
                "Code mode resume diverged at setTimeout call {}: expected {}ms, got {}ms.",
                call_index + 1,
                recorded.delay_ms,
                delay_ms,
            ));
        }

        let action = if recorded.cleared || recorded.completed {
            TimerAction::Done
        } else if now_ms >= recorded.due_at_ms {
            TimerAction::Run
        } else {
            TimerAction::Pending
        };
        let remaining_ms = matches!(action, TimerAction::Pending)
            .then_some(recorded.due_at_ms.saturating_sub(now_ms));

        return Ok(TimerRegistration {
            timer_id: recorded.timer_id.clone(),
            action,
            remaining_ms,
        });
    }

    let timer_id = format!("timer_{}", timer_calls.len() + 1);
    let due_at_ms = now_ms.saturating_add(delay_ms);
    // Only zero-delay timers run immediately; others remain pending.
    let action = if delay_ms == 0 {
        TimerAction::Run
    } else {
        TimerAction::Pending
    };
    let remaining_ms =
        matches!(action, TimerAction::Pending).then_some(due_at_ms.saturating_sub(now_ms));

    timer_calls.push(RecordedTimerCall {
        timer_id: timer_id.clone(),
        delay_ms,
        due_at_ms,
        completed: false,
        cleared: false,
    });

    Ok(TimerRegistration {
        timer_id,
        action,
        remaining_ms,
    })
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
    let mut completed_ids = Vec::new();

    for timer in timer_calls.iter() {
        if timer.cleared || timer.completed {
            completed_ids.push(timer.timer_id.clone());
            continue;
        }

        pending_timers += 1;
        let remaining_ms = timer.due_at_ms.saturating_sub(now_ms);
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
        completed_ids,
    }
}

pub fn due_timers(timer_calls: &[RecordedTimerCall], now_ms: u64) -> Vec<String> {
    timer_calls
        .iter()
        .filter(|timer| !timer.cleared && !timer.completed && now_ms >= timer.due_at_ms)
        .map(|timer| timer.timer_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_timeout_reuses_existing_calls_and_tracks_pending_state() {
        let mut calls = Vec::new();
        let first = register_timeout(&mut calls, 0, 25, 1_000).expect("initial timeout");
        assert_eq!(first.timer_id, "timer_1");
        assert_eq!(first.action, TimerAction::Pending);
        assert_eq!(first.remaining_ms, Some(25));

        let replayed = register_timeout(&mut calls, 0, 25, 1_010).expect("replayed timeout");
        assert_eq!(replayed.timer_id, "timer_1");
        assert_eq!(replayed.action, TimerAction::Pending);
        assert_eq!(replayed.remaining_ms, Some(15));

        let pending = pending_timer_state(&calls, 1_010);
        assert!(pending.has_pending_timers());
        assert_eq!(pending.pending_timers, 1);
        assert_eq!(pending.next_timer_id.as_deref(), Some("timer_1"));
        assert_eq!(pending.resume_after_ms, Some(15));
    }

    #[test]
    fn clear_and_complete_remove_timers_from_pending_state() {
        let mut calls = Vec::new();
        let first = register_timeout(&mut calls, 0, 0, 1_000).expect("immediate timeout");
        assert_eq!(first.action, TimerAction::Run);
        mark_timeout_completed(&mut calls, &first.timer_id);
        assert!(!pending_timer_state(&calls, 1_000).has_pending_timers());

        let second = register_timeout(&mut calls, 1, 50, 1_000).expect("delayed timeout");
        assert_eq!(second.action, TimerAction::Pending);
        clear_timeout(&mut calls, &second.timer_id);
        assert!(!pending_timer_state(&calls, 1_010).has_pending_timers());
    }
}
