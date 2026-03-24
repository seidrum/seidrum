//! Tool loop guardrails: turn limits, loop detection, time limits.

use std::collections::HashMap;
use std::time::Instant;

use seidrum_common::events::GuardrailConfig;

/// Tracks guardrail state during a consciousness event processing cycle.
pub struct GuardrailState {
    pub turn_count: u32,
    pub start_time: Instant,
    tool_call_history: HashMap<String, u32>, // hash of (tool_name, args) → count
    config: GuardrailConfig,
}

/// Reason a guardrail was triggered.
#[derive(Debug)]
pub enum GuardrailViolation {
    TurnLimitReached(u32),
    TimeLimitReached(u64),
    LoopDetected { tool_name: String, count: u32 },
    HitlRequired(u32),
}

impl GuardrailState {
    pub fn new(config: GuardrailConfig) -> Self {
        Self {
            turn_count: 0,
            start_time: Instant::now(),
            tool_call_history: HashMap::new(),
            config,
        }
    }

    /// Record a tool call turn and return a violation if guardrails are hit.
    pub fn record_turn(
        &mut self,
        tool_name: &str,
        arguments_hash: &str,
    ) -> Option<GuardrailViolation> {
        self.turn_count += 1;

        // Turn limit
        if self.turn_count >= self.config.turn_limit {
            return Some(GuardrailViolation::TurnLimitReached(self.turn_count));
        }

        // Time limit
        let elapsed = self.start_time.elapsed().as_secs();
        if elapsed >= self.config.time_limit_seconds {
            return Some(GuardrailViolation::TimeLimitReached(elapsed));
        }

        // Loop detection
        let key = format!("{}:{}", tool_name, arguments_hash);
        let count = self.tool_call_history.entry(key).or_insert(0);
        *count += 1;
        if *count >= self.config.loop_detection_threshold {
            return Some(GuardrailViolation::LoopDetected {
                tool_name: tool_name.to_string(),
                count: *count,
            });
        }

        // Human-in-the-loop (triggers once when threshold is crossed)
        if let Some(hitl_after) = self.config.hitl_after_turns {
            if self.turn_count >= hitl_after
                && (self.turn_count == hitl_after || self.turn_count.saturating_sub(1) < hitl_after)
            {
                return Some(GuardrailViolation::HitlRequired(self.turn_count));
            }
        }

        None
    }

    /// Add tool rounds from a provider response (for tracking external loops).
    pub fn add_provider_rounds(&mut self, rounds: u32) {
        self.turn_count += rounds;
    }
}

/// Default guardrails for user conversations.
pub fn default_user_conversation() -> GuardrailConfig {
    GuardrailConfig {
        turn_limit: 50,
        time_limit_seconds: 600,
        loop_detection_threshold: 3,
        hitl_after_turns: Some(20),
    }
}

/// Default guardrails for consciousness event processing.
pub fn default_consciousness() -> GuardrailConfig {
    GuardrailConfig {
        turn_limit: 100,
        time_limit_seconds: 1800,
        loop_detection_threshold: 3,
        hitl_after_turns: None,
    }
}

/// Default guardrails for delegated tasks.
pub fn default_delegation() -> GuardrailConfig {
    GuardrailConfig {
        turn_limit: 200,
        time_limit_seconds: 3600,
        loop_detection_threshold: 3,
        hitl_after_turns: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_limit_triggers() {
        let config = GuardrailConfig {
            turn_limit: 3,
            time_limit_seconds: 600,
            loop_detection_threshold: 10,
            hitl_after_turns: None,
        };
        let mut state = GuardrailState::new(config);

        assert!(state.record_turn("tool-a", "args1").is_none());
        assert!(state.record_turn("tool-b", "args2").is_none());
        assert!(matches!(
            state.record_turn("tool-c", "args3"),
            Some(GuardrailViolation::TurnLimitReached(3))
        ));
    }

    #[test]
    fn loop_detection_triggers() {
        let config = GuardrailConfig {
            turn_limit: 100,
            time_limit_seconds: 600,
            loop_detection_threshold: 2,
            hitl_after_turns: None,
        };
        let mut state = GuardrailState::new(config);

        assert!(state.record_turn("tool-a", "same-args").is_none());
        assert!(matches!(
            state.record_turn("tool-a", "same-args"),
            Some(GuardrailViolation::LoopDetected { .. })
        ));
    }

    #[test]
    fn hitl_triggers() {
        let config = GuardrailConfig {
            turn_limit: 100,
            time_limit_seconds: 600,
            loop_detection_threshold: 10,
            hitl_after_turns: Some(2),
        };
        let mut state = GuardrailState::new(config);

        assert!(state.record_turn("tool-a", "args1").is_none());
        assert!(matches!(
            state.record_turn("tool-b", "args2"),
            Some(GuardrailViolation::HitlRequired(2))
        ));
    }

    #[test]
    fn provider_rounds_count() {
        let config = GuardrailConfig {
            turn_limit: 10,
            time_limit_seconds: 600,
            loop_detection_threshold: 10,
            hitl_after_turns: None,
        };
        let mut state = GuardrailState::new(config);
        state.add_provider_rounds(8);
        assert!(state.record_turn("tool-a", "args1").is_none()); // turn 9
        assert!(matches!(
            state.record_turn("tool-b", "args2"),
            Some(GuardrailViolation::TurnLimitReached(10))
        ));
    }
}
