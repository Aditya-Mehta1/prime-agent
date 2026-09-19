//! The goal driver: goal-state lifecycle, usage accounting, budget limits,
//! and continuation context. Port of the goal machinery in agent-session.ts
//! (the `_goalState` half), with persistence via `thread_goal_state` custom
//! entries and the branch-seed/reload rules.

use pa_types::session::{CustomMessage, FileEntry};

use crate::goals::{
    create_goal_context_message, empty_goal_state, goal_token_delta_for_usage,
    is_persisted_goal_state, normalize_goal_state, validate_goal_budget, validate_goal_objective,
    GoalContextKind, GoalState, GoalStatus, GOAL_STATE_CUSTOM_TYPE,
};
use crate::session::manager::SessionManager;

/// Wall-clock accounting anchor for time-used attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountingStartedAt(pub u64);

/// What happened after accounting one assistant turn's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageOutcome {
    Accounted,
    /// The goal hit its token budget and moved to `budget_limited`.
    BudgetReached,
    /// The goal was not active; usage was ignored.
    Ignored,
}

pub struct GoalDriver {
    state: GoalState,
    accounting_started_at: Option<AccountingStartedAt>,
    /// Ids of assistant messages already counted (double-counting guard).
    accounted_messages: std::collections::HashSet<String>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl GoalDriver {
    pub fn new() -> Self {
        Self {
            state: empty_goal_state(),
            accounting_started_at: None,
            accounted_messages: Default::default(),
        }
    }

    /// Rehydrate the driver from the session branch (latest persisted entry).
    pub fn load_persisted(session: &SessionManager) -> Self {
        let mut state = empty_goal_state();
        for entry in session.get_all_entries().iter().rev() {
            if let FileEntry::Custom { payload, .. } = entry {
                if payload.custom_type == GOAL_STATE_CUSTOM_TYPE {
                    if let Some(data) = &payload.data {
                        if is_persisted_goal_state(data) {
                            if let Ok(parsed) = serde_json::from_value::<GoalState>(data.clone()) {
                                state = normalize_goal_state(parsed);
                                break;
                            }
                        }
                    }
                }
            }
        }
        let accounting_started_at =
            (state.status == GoalStatus::Active).then_some(AccountingStartedAt(now_millis()));
        Self {
            state,
            accounting_started_at,
            accounted_messages: Default::default(),
        }
    }

    pub fn state(&self) -> &GoalState {
        &self.state
    }

    /// Whether the branch may be seeded with an initial goal: only bootstrap
    /// entries (model/thinking changes) and no prior persisted goal.
    pub fn is_branch_seedable(session: &SessionManager) -> bool {
        for entry in session.get_all_entries() {
            match entry {
                // Bootstrap entries (and the header line) do not block seeding.
                FileEntry::Header { .. }
                | FileEntry::ModelChange { .. }
                | FileEntry::ThinkingLevelChange { .. }
                | FileEntry::ServiceTierChange { .. } => continue,
                _ => return false,
            }
        }
        true
    }

    /// Start a new goal (validates objective and budget).
    pub fn start(
        &mut self,
        session: &mut SessionManager,
        objective_text: &str,
        token_budget: Option<u64>,
    ) -> anyhow::Result<GoalState> {
        let objective = validate_goal_objective(objective_text)?;
        let budget = validate_goal_budget(token_budget)?;
        let now = now_millis();
        let goal = GoalState {
            active: true,
            status: GoalStatus::Active,
            goal_id: Some(uuid::Uuid::new_v4().to_string()),
            objective: Some(objective),
            token_budget: budget,
            tokens_used: 0,
            time_used_seconds: 0,
            continuations_used: 0,
            created_at: Some(now),
            updated_at: Some(now),
            last_reason: None,
            last_error: None,
        };
        self.accounting_started_at = Some(AccountingStartedAt(now));
        self.accounted_messages.clear();
        self.set_state(session, goal);
        Ok(self.state.clone())
    }

    /// Clear the goal entirely (empty state).
    pub fn clear(&mut self, session: &mut SessionManager) {
        self.set_state(session, empty_goal_state());
        self.accounting_started_at = None;
    }

    /// Time-used attribution: fold wall-clock time since accounting started.
    fn with_accounted_wall_clock(&self) -> GoalState {
        let Some(started) = self.accounting_started_at else {
            return self.state.clone();
        };
        let elapsed_seconds = now_millis().saturating_sub(started.0) / 1000;
        GoalState {
            time_used_seconds: self.state.time_used_seconds + elapsed_seconds,
            ..self.state.clone()
        }
    }

    fn set_state(&mut self, session: &mut SessionManager, next: GoalState) {
        let normalized = normalize_goal_state(GoalState {
            updated_at: Some(now_millis()),
            ..next
        });
        if normalized.status == GoalStatus::Active {
            self.accounting_started_at
                .get_or_insert(AccountingStartedAt(now_millis()));
        } else {
            self.accounting_started_at = None;
        }
        if let Ok(value) = serde_json::to_value(&normalized) {
            session.append_custom_entry(GOAL_STATE_CUSTOM_TYPE, Some(value));
            session.flush_now();
        }
        self.state = normalized;
    }

    /// Account one assistant turn's usage. Double-counts are suppressed by
    /// message id. Returns whether the budget was reached.
    pub fn record_assistant_usage(
        &mut self,
        session: &mut SessionManager,
        message_id: &str,
        usage: &pa_types::ai::Usage,
    ) -> UsageOutcome {
        if self.state.status != GoalStatus::Active {
            return UsageOutcome::Ignored;
        }
        if !self.accounted_messages.insert(message_id.to_string()) {
            return UsageOutcome::Ignored;
        }
        let token_delta = goal_token_delta_for_usage(usage.input as i64, usage.output as i64);
        let goal = self.with_accounted_wall_clock();
        let next_goal = GoalState {
            tokens_used: goal.tokens_used + token_delta,
            ..goal
        };
        let budget_reached = next_goal
            .token_budget
            .is_some_and(|budget| next_goal.tokens_used >= budget);
        if !budget_reached {
            let budget = next_goal.token_budget;
            self.set_state(session, next_goal);
            let _ = budget;
            return UsageOutcome::Accounted;
        }
        let token_budget = next_goal.token_budget;
        let budget_reason = token_budget
            .map(|budget| format!("Reached {budget} token goal budget"))
            .unwrap_or_default();
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::BudgetLimited,
                last_reason: Some(budget_reason),
                last_error: None,
                ..next_goal
            },
        );
        UsageOutcome::BudgetReached
    }

    /// Pause the goal (no-op when not active).
    pub fn pause(&mut self, session: &mut SessionManager, reason: &str) {
        if self.state.status != GoalStatus::Active {
            return;
        }
        let goal = self.with_accounted_wall_clock();
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::Paused,
                last_reason: Some(reason.to_string()),
                last_error: None,
                ..goal
            },
        );
    }

    /// Resume a paused/budget-limited goal. Returns the continuation context
    /// message when the goal becomes active again.
    pub fn resume(&mut self, session: &mut SessionManager) -> Option<CustomMessage> {
        self.state.objective.as_ref()?;
        if !matches!(
            self.state.status,
            GoalStatus::Paused | GoalStatus::BudgetLimited
        ) {
            return None;
        }
        let exhausted = self
            .state
            .token_budget
            .is_some_and(|budget| self.state.tokens_used >= budget);
        let next_status = if exhausted {
            GoalStatus::BudgetLimited
        } else {
            GoalStatus::Active
        };
        self.set_state(
            session,
            GoalState {
                active: next_status == GoalStatus::Active,
                status: next_status,
                // TS `_resumeGoal`: the reason is only set for an exhausted
                // budget (which stays budget_limited); a live resume clears it.
                last_reason: exhausted.then(|| "Goal token budget already reached".to_string()),
                last_error: None,
                ..self.state.clone()
            },
        );
        if next_status == GoalStatus::Active {
            return create_goal_context_message(&self.state, GoalContextKind::Continuation).ok();
        }
        None
    }

    /// Complete the goal (host `goal.complete()`).
    pub fn complete(&mut self, session: &mut SessionManager) {
        if self.state.objective.is_none() || self.state.status == GoalStatus::Idle {
            return;
        }
        let goal = self.with_accounted_wall_clock();
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::Complete,
                last_reason: Some("Goal achieved".to_string()),
                last_error: None,
                ..goal
            },
        );
    }

    /// Terminal-assistant handling: `aborted` keeps the goal, `error` fails it.
    pub fn finish_for_terminal_message(
        &mut self,
        session: &mut SessionManager,
        stop_reason: pa_types::ai::StopReason,
        error_message: Option<&str>,
    ) {
        if self.state.status != GoalStatus::Active {
            return;
        }
        use pa_types::ai::StopReason;
        match stop_reason {
            StopReason::Aborted => {}
            StopReason::Error => {
                let reason = error_message
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Assistant response failed");
                let goal = self.with_accounted_wall_clock();
                self.set_state(
                    session,
                    GoalState {
                        active: false,
                        status: GoalStatus::Error,
                        last_reason: Some(reason.to_string()),
                        last_error: Some(reason.to_string()),
                        ..goal
                    },
                );
            }
            _ => {}
        }
    }

    /// Build the next continuation context, consuming one continuation slot.
    pub fn next_continuation_message(&mut self) -> Option<CustomMessage> {
        if self.state.status != GoalStatus::Active || self.state.objective.is_none() {
            return None;
        }
        self.state = normalize_goal_state(GoalState {
            continuations_used: self.state.continuations_used + 1,
            last_reason: None,
            last_error: None,
            updated_at: Some(now_millis()),
            ..self.state.clone()
        });
        create_goal_context_message(&self.state, GoalContextKind::Continuation).ok()
    }

    /// Whether the goal drives session wake-ups.
    pub fn owns_continuation_wakeup(&self) -> bool {
        self.state.status == GoalStatus::Active && self.state.objective.is_some()
    }

    /// The active objective, when set and active.
    pub fn active_objective(&self) -> Option<String> {
        (self.state.status == GoalStatus::Active)
            .then(|| self.state.objective.clone())
            .flatten()
    }
}

impl Default for GoalDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::MAX_THREAD_GOAL_OBJECTIVE_CHARS;
    use crate::session::manager::SessionManager;
    use pa_types::ai::UserContent;

    fn persisted_session() -> SessionManager {
        let dir = tempfile::TempDir::new().unwrap();
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut session = SessionManager::in_memory(dir.path());
        session.materialize_session_file(Some(session_dir));
        session
    }

    fn usage(input: u64, output: u64) -> pa_types::ai::Usage {
        pa_types::ai::Usage {
            input,
            output,
            ..Default::default()
        }
    }

    #[test]
    fn start_resume_pause_lifecycle() {
        let mut session = persisted_session();
        let mut driver = GoalDriver::new();
        assert_eq!(driver.state(), &empty_goal_state());
        let goal = driver
            .start(&mut session, "  ship the mission  ", Some(1000))
            .unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.objective.as_deref(), Some("ship the mission"));
        assert_eq!(goal.token_budget, Some(1000));
        assert!(goal.goal_id.is_some());
        assert!(driver.owns_continuation_wakeup());
        // Validation errors.
        assert!(driver.start(&mut session, "", None).is_err());
        let long = "x".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS + 1);
        assert!(driver.start(&mut session, &long, None).is_err());
        assert!(driver.start(&mut session, "ok", Some(0)).is_err());
        // Pause keeps the objective; resume returns a continuation context.
        driver.pause(&mut session, "Paused by user");
        assert_eq!(driver.state().status, GoalStatus::Paused);
        assert!(!driver.owns_continuation_wakeup());
        let continuation = driver.resume(&mut session).unwrap();
        assert_eq!(
            continuation.custom_type,
            crate::goals::GOAL_CONTEXT_CUSTOM_TYPE
        );
        let UserContent::Text(text) = &continuation.content else {
            panic!("expected text content");
        };
        assert!(text.starts_with("[goal: continuation]"));
        // Rehydrating from the session restores the active goal.
        let reloaded = GoalDriver::load_persisted(&session);
        assert_eq!(reloaded.state().status, GoalStatus::Active);
        assert_eq!(
            reloaded.state().objective.as_deref(),
            Some("ship the mission")
        );
        // Clear resets everything.
        driver.clear(&mut session);
        assert_eq!(driver.state().status, GoalStatus::Idle);
        assert_eq!(driver.state().objective, None);
    }

    #[test]
    fn usage_accounting_and_budget_limit() {
        let mut session = persisted_session();
        let mut driver = GoalDriver::new();
        driver.start(&mut session, "work", Some(100)).unwrap();
        assert_eq!(
            driver.record_assistant_usage(&mut session, "a1", &usage(30, 10)),
            UsageOutcome::Accounted
        );
        assert_eq!(driver.state().tokens_used, 40);
        // Double-counting the same message is ignored.
        assert_eq!(
            driver.record_assistant_usage(&mut session, "a1", &usage(30, 10)),
            UsageOutcome::Ignored
        );
        assert_eq!(driver.state().tokens_used, 40);
        // Budget reached transitions to budget_limited.
        assert_eq!(
            driver.record_assistant_usage(&mut session, "a2", &usage(50, 10)),
            UsageOutcome::BudgetReached
        );
        assert_eq!(driver.state().status, GoalStatus::BudgetLimited);
        assert_eq!(
            driver.state().last_reason.as_deref(),
            Some("Reached 100 token goal budget")
        );
        // Usage while inactive is ignored.
        assert_eq!(
            driver.record_assistant_usage(&mut session, "a3", &usage(50, 10)),
            UsageOutcome::Ignored
        );
        // Resuming an exhausted goal stays budget_limited.
        assert!(driver.resume(&mut session).is_none());
        assert_eq!(driver.state().status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn terminal_messages_fail_or_keep_the_goal() {
        use pa_types::ai::StopReason;
        let mut session = persisted_session();
        let mut driver = GoalDriver::new();
        driver.start(&mut session, "work", None).unwrap();
        // Aborted keeps the goal active.
        driver.finish_for_terminal_message(&mut session, StopReason::Aborted, None);
        assert_eq!(driver.state().status, GoalStatus::Active);
        // Error fails it with the provided message.
        driver.finish_for_terminal_message(
            &mut session,
            StopReason::Error,
            Some("provider exploded"),
        );
        assert_eq!(driver.state().status, GoalStatus::Error);
        assert_eq!(
            driver.state().last_error.as_deref(),
            Some("provider exploded")
        );
        // Terminal handling is inert when the goal is not active.
        driver.finish_for_terminal_message(&mut session, StopReason::Error, None);
        assert_eq!(driver.state().status, GoalStatus::Error);
    }

    #[test]
    fn continuations_increment() {
        let mut session = persisted_session();
        let mut driver = GoalDriver::new();
        driver.start(&mut session, "work", None).unwrap();
        let first = driver.next_continuation_message().unwrap();
        let UserContent::Text(text) = &first.content else {
            panic!("expected text content");
        };
        assert!(text.contains("- status: active"));
        assert_eq!(driver.state().continuations_used, 1);
        assert!(driver.next_continuation_message().is_some());
        assert_eq!(driver.state().continuations_used, 2);
        // Inactive goals produce no continuations.
        driver.pause(&mut session, "Paused by user");
        assert!(driver.next_continuation_message().is_none());
    }

    /// TS `_resumeGoal` semantics: resume continues the same goal (same
    /// id and objective, no re-creation) and only sets a reason when the
    /// budget is already exhausted.
    #[test]
    fn resume_resolves_the_existing_goal() {
        let mut session = persisted_session();
        let mut driver = GoalDriver::new();
        driver.start(&mut session, "ship it", None).unwrap();
        driver.pause(&mut session, "Paused by user");
        let paused = driver.state().clone();
        driver.resume(&mut session).unwrap();
        assert_eq!(driver.state().goal_id, paused.goal_id);
        assert_eq!(driver.state().objective.as_deref(), Some("ship it"));
        assert_eq!(driver.state().status, GoalStatus::Active);
        assert!(driver.state().last_reason.is_none());
        // An exhausted budget stays budget_limited with the TS reason.
        let mut limited = persisted_session();
        let mut driver = GoalDriver::new();
        driver.start(&mut limited, "ship it", Some(100)).unwrap();
        driver.record_assistant_usage(&mut limited, "a1", &usage(120, 0));
        assert_eq!(driver.state().status, GoalStatus::BudgetLimited);
        assert!(driver.resume(&mut limited).is_none());
        assert_eq!(driver.state().status, GoalStatus::BudgetLimited);
        assert_eq!(
            driver.state().last_reason.as_deref(),
            Some("Goal token budget already reached")
        );
    }

    #[test]
    fn branch_seedable_rules() {
        let mut session = persisted_session();
        assert!(GoalDriver::is_branch_seedable(&session));
        // A persisted goal means the branch is not seedable.
        let mut driver = GoalDriver::new();
        driver.start(&mut session, "work", None).unwrap();
        assert!(!GoalDriver::is_branch_seedable(&session));
        // Messages also block seeding.
        let mut other = persisted_session();
        other.append_message(pa_types::session::AgentMessage::User(
            pa_types::ai::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
                rest: Default::default(),
            },
        ));
        assert!(!GoalDriver::is_branch_seedable(&other));
    }
}
