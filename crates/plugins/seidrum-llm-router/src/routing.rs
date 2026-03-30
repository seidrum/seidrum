/// Intelligent LLM provider routing.
///
/// Determines which LLM provider should handle a request based on:
/// - Model preferences in the request
/// - Request characteristics (tool count, message count, estimated tokens)
/// - Scope (privacy-sensitive scopes route to local providers)
/// - Routing strategy hints
///
/// Supports fixed routing, fallback lists, and intelligent profile-based routing.

use serde::{Deserialize, Serialize};
use tracing::debug;

/// Available LLM providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Google,
    OpenAI,
    Anthropic,
    Ollama,
}

impl Provider {
    /// Get the NATS subject for this provider.
    pub fn nats_subject(&self) -> String {
        match self {
            Provider::Google => "llm.provider.google".to_string(),
            Provider::OpenAI => "llm.provider.openai".to_string(),
            Provider::Anthropic => "llm.provider.anthropic".to_string(),
            Provider::Ollama => "llm.provider.ollama".to_string(),
        }
    }

    /// Get the provider name as a string.
    pub fn name(&self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::OpenAI => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Ollama => "ollama",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Routing strategy determines how the router selects a provider.
#[derive(Debug, Clone)]
pub enum RoutingStrategy {
    /// Always use a specific provider.
    Fixed(Provider),
    /// Try providers in order, fallback on failure.
    Fallback(Vec<Provider>),
    /// Route based on task characteristics.
    Intelligent(IntelligentRoutingConfig),
}

/// Configuration for intelligent routing based on request characteristics.
#[derive(Debug, Clone)]
pub struct IntelligentRoutingConfig {
    /// Provider for tool-heavy requests (many tools, likely to need tool calls).
    pub tool_heavy: Provider,
    /// Provider for simple/fast queries.
    pub simple: Provider,
    /// Provider for complex reasoning.
    pub complex: Provider,
    /// Provider for privacy-sensitive scopes.
    pub private: Provider,
    /// Fallback if primary choice fails.
    pub fallback: Provider,
}

impl Default for IntelligentRoutingConfig {
    fn default() -> Self {
        Self {
            tool_heavy: Provider::Google,    // Gemini is good at tool calling
            simple: Provider::Ollama,        // Local = fast + free
            complex: Provider::Anthropic,    // Claude for reasoning
            private: Provider::Ollama,       // Local = no data leaves machine
            fallback: Provider::Google,
        }
    }
}

/// Characteristics of a request used for routing decisions.
pub struct RequestProfile {
    /// Number of tools available for this request.
    pub tool_count: usize,
    /// Number of messages in the conversation.
    pub message_count: usize,
    /// Estimated total tokens for the request.
    pub estimated_tokens: usize,
    /// Whether a system prompt is present.
    #[allow(dead_code)]
    pub has_system_prompt: bool,
    /// Scope from the event envelope (e.g., "personal", "medical", "work").
    pub scope: Option<String>,
    /// Routing strategy hint from the request (e.g., "private", "local", "best-first").
    pub routing_strategy: String,
    /// Model preferences from the request (e.g., ["claude-3", "gpt-4"]).
    pub model_preferences: Vec<String>,
}

/// Select the best provider for a given request profile.
///
/// Decision logic:
/// 1. Check model preferences first — explicit preference overrides routing
/// 2. Check routing strategy hint and scope for privacy requirements
/// 3. Route by request characteristics (tool-heavy, simple, complex)
/// 4. Use fallback as the default
pub fn select_provider(
    profile: &RequestProfile,
    config: &RoutingStrategy,
) -> Provider {
    match config {
        RoutingStrategy::Fixed(p) => p.clone(),
        RoutingStrategy::Fallback(providers) => {
            providers.first().cloned().unwrap_or(Provider::Google)
        }
        RoutingStrategy::Intelligent(ic) => select_intelligent(profile, ic),
    }
}

/// Intelligent routing logic.
fn select_intelligent(profile: &RequestProfile, config: &IntelligentRoutingConfig) -> Provider {
    // 1. Check model preferences first — explicit preference overrides routing
    for pref in &profile.model_preferences {
        let lower = pref.to_lowercase();
        if lower.contains("gpt") || lower.contains("openai") {
            debug!(preference = %pref, "Routing to OpenAI per model preference");
            return Provider::OpenAI;
        }
        if lower.contains("claude") || lower.contains("anthropic") {
            debug!(preference = %pref, "Routing to Anthropic per model preference");
            return Provider::Anthropic;
        }
        if lower.contains("gemini") || lower.contains("google") {
            debug!(preference = %pref, "Routing to Google per model preference");
            return Provider::Google;
        }
        if lower.contains("llama")
            || lower.contains("ollama")
            || lower.contains("local")
            || lower.contains("mistral")
        {
            debug!(preference = %pref, "Routing to Ollama per model preference");
            return Provider::Ollama;
        }
    }

    // 2. Check routing strategy hint
    match profile.routing_strategy.as_str() {
        "private" | "local" => {
            debug!(strategy = %profile.routing_strategy, "Routing to local provider for privacy");
            return config.private.clone();
        }
        _ => {}
    }

    // 3. Check scope for privacy-sensitive keywords
    if let Some(scope) = &profile.scope {
        let scope_lower = scope.to_lowercase();
        if scope_lower.contains("private")
            || scope_lower.contains("personal")
            || scope_lower.contains("medical")
            || scope_lower.contains("financial")
            || scope_lower.contains("health")
            || scope_lower.contains("legal")
        {
            debug!(scope = %scope, "Routing to local provider for sensitive scope");
            return config.private.clone();
        }
    }

    // 4. Route by request characteristics
    if profile.tool_count > 3 {
        debug!(tool_count = profile.tool_count, "Routing to tool-heavy provider");
        return config.tool_heavy.clone();
    }

    if profile.estimated_tokens > 10000 || profile.message_count > 20 {
        debug!(
            estimated_tokens = profile.estimated_tokens,
            message_count = profile.message_count,
            "Routing to complex reasoning provider"
        );
        return config.complex.clone();
    }

    // 5. Simple queries go to fastest/cheapest
    if profile.estimated_tokens < 1000 && profile.tool_count == 0 {
        debug!("Routing to simple/fast provider");
        return config.simple.clone();
    }

    // 6. Default to fallback
    debug!("Using fallback provider");
    config.fallback.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_nats_subject() {
        assert_eq!(Provider::Google.nats_subject(), "llm.provider.google");
        assert_eq!(Provider::OpenAI.nats_subject(), "llm.provider.openai");
        assert_eq!(Provider::Anthropic.nats_subject(), "llm.provider.anthropic");
        assert_eq!(Provider::Ollama.nats_subject(), "llm.provider.ollama");
    }

    #[test]
    fn test_provider_name() {
        assert_eq!(Provider::Google.name(), "google");
        assert_eq!(Provider::OpenAI.name(), "openai");
        assert_eq!(Provider::Anthropic.name(), "anthropic");
        assert_eq!(Provider::Ollama.name(), "ollama");
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(Provider::Google.to_string(), "google");
        assert_eq!(Provider::OpenAI.to_string(), "openai");
    }

    #[test]
    fn test_fixed_routing() {
        let config = RoutingStrategy::Fixed(Provider::Anthropic);
        let profile = RequestProfile {
            tool_count: 10,
            message_count: 50,
            estimated_tokens: 50000,
            has_system_prompt: true,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Anthropic);
    }

    #[test]
    fn test_fallback_routing() {
        let config = RoutingStrategy::Fallback(vec![
            Provider::OpenAI,
            Provider::Anthropic,
            Provider::Google,
        ]);
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "fallback".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::OpenAI);
    }

    #[test]
    fn test_model_preference_openai() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["gpt-4".to_string()],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::OpenAI);
    }

    #[test]
    fn test_model_preference_claude() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["Claude-3-Opus".to_string()],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Anthropic);
    }

    #[test]
    fn test_model_preference_ollama() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["local".to_string()],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }

    #[test]
    fn test_routing_strategy_private() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "private".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }

    #[test]
    fn test_scope_private_keyword() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: Some("scope_personal".to_string()),
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }

    #[test]
    fn test_scope_medical_keyword() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: Some("medical_records".to_string()),
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }

    #[test]
    fn test_tool_heavy_request() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 5,
            message_count: 5,
            estimated_tokens: 2000,
            has_system_prompt: true,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Google);
    }

    #[test]
    fn test_complex_reasoning_request() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 25,
            estimated_tokens: 15000,
            has_system_prompt: true,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Anthropic);
    }

    #[test]
    fn test_simple_request() {
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 500,
            has_system_prompt: false,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }

    #[test]
    fn test_intelligent_routing_default_config() {
        let config = IntelligentRoutingConfig::default();
        assert_eq!(config.tool_heavy, Provider::Google);
        assert_eq!(config.simple, Provider::Ollama);
        assert_eq!(config.complex, Provider::Anthropic);
        assert_eq!(config.private, Provider::Ollama);
        assert_eq!(config.fallback, Provider::Google);
    }

    #[test]
    fn test_preference_overrides_characteristics() {
        // Even though it's tool-heavy (should go to Google),
        // explicit preference for Claude should override.
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 10,
            message_count: 5,
            estimated_tokens: 5000,
            has_system_prompt: true,
            scope: None,
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["Claude".to_string()],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Anthropic);
    }

    #[test]
    fn test_privacy_scope_overrides_characteristics() {
        // Even though it's simple (should go to Ollama),
        // privacy scope should still route to local provider.
        let config = RoutingStrategy::Intelligent(IntelligentRoutingConfig::default());
        let profile = RequestProfile {
            tool_count: 0,
            message_count: 1,
            estimated_tokens: 100,
            has_system_prompt: false,
            scope: Some("financial_data".to_string()),
            routing_strategy: "best-first".to_string(),
            model_preferences: vec![],
        };

        let selected = select_provider(&profile, &config);
        assert_eq!(selected, Provider::Ollama);
    }
}
