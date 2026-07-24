use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const DEFAULT_SUMMARY_INPUT_BUDGET: usize = 236_000;
pub(crate) const MIN_SUMMARY_INPUT_BUDGET: usize = 4_000;
pub(crate) const MAX_SUMMARY_INPUT_BUDGET: usize = DEFAULT_SUMMARY_INPUT_BUDGET;
pub(crate) const DEFAULT_SUMMARY_MAX_OUTPUT_TOKENS: usize = 12_000;
pub(crate) const MIN_SUMMARY_MAX_OUTPUT_TOKENS: usize = 1_200;
pub(crate) const MAX_SUMMARY_MAX_OUTPUT_TOKENS: usize = DEFAULT_SUMMARY_MAX_OUTPUT_TOKENS;

pub(crate) const ENVELOPE_PREFIX: &str = "bcmp1.";
pub(crate) const ENVELOPE_VERSION: i64 = 1;
pub(crate) const KEY_VERSION: i64 = 1;
pub(crate) const MIGRATION_PROMPT_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionRolloutMode {
    Off,
    ObserveOnly,
    ThirdPartyOnly,
    #[default]
    FullSwitching,
}

impl CompactionRolloutMode {
    pub(crate) fn allows_bridge_compaction(self) -> bool {
        matches!(self, Self::ThirdPartyOnly | Self::FullSwitching)
    }

    pub(crate) fn allows_cross_realm(self) -> bool {
        self == Self::FullSwitching
    }

    pub(crate) fn captures_official(self) -> bool {
        self != Self::Off
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(default)]
    pub rollout_mode: CompactionRolloutMode,
    #[serde(default = "default_true")]
    pub official_compact_fallback: bool,
    /// Optional, pinned third-party Codex provider used only for local bridge
    /// summaries. `None` preserves the existing active-route/failover behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_provider_id: Option<String>,
    /// Exact upstream model for bridge summaries. This is meaningful only when
    /// `summary_provider_id` is set; `None` uses that provider's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// Initial per-call input budget. Overflow retries reduce this automatically.
    #[serde(default = "default_summary_input_budget")]
    pub summary_input_budget: usize,
    /// Final handoff output ceiling. Chunk summaries remain derived and bounded.
    #[serde(default = "default_summary_max_output_tokens")]
    pub summary_max_output_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            rollout_mode: CompactionRolloutMode::FullSwitching,
            official_compact_fallback: true,
            summary_provider_id: None,
            summary_model: None,
            summary_input_budget: DEFAULT_SUMMARY_INPUT_BUDGET,
            summary_max_output_tokens: DEFAULT_SUMMARY_MAX_OUTPUT_TOKENS,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_summary_input_budget() -> usize {
    DEFAULT_SUMMARY_INPUT_BUDGET
}

fn default_summary_max_output_tokens() -> usize {
    DEFAULT_SUMMARY_MAX_OUTPUT_TOKENS
}

impl CompactionSettings {
    pub(crate) fn normalized(mut self) -> Self {
        self.summary_provider_id = normalize_optional_text(self.summary_provider_id);
        self.summary_model = normalize_optional_text(self.summary_model);
        if self.summary_provider_id.is_none() {
            self.summary_model = None;
        }
        self
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(MIN_SUMMARY_INPUT_BUDGET..=MAX_SUMMARY_INPUT_BUDGET)
            .contains(&self.summary_input_budget)
        {
            return Err(format!(
                "summary input budget must be between {MIN_SUMMARY_INPUT_BUDGET} and {MAX_SUMMARY_INPUT_BUDGET} tokens"
            ));
        }
        if !(MIN_SUMMARY_MAX_OUTPUT_TOKENS..=MAX_SUMMARY_MAX_OUTPUT_TOKENS)
            .contains(&self.summary_max_output_tokens)
        {
            return Err(format!(
                "summary output limit must be between {MIN_SUMMARY_MAX_OUTPUT_TOKENS} and {MAX_SUMMARY_MAX_OUTPUT_TOKENS} tokens"
            ));
        }
        if self
            .summary_provider_id
            .as_deref()
            .is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
        {
            return Err("summary provider id is invalid".to_string());
        }
        if self
            .summary_model
            .as_deref()
            .is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
        {
            return Err("summary model is invalid".to_string());
        }
        if self.summary_provider_id.is_none() && self.summary_model.is_some() {
            return Err("summary model requires a pinned summary provider".to_string());
        }
        Ok(())
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderRealm {
    Official,
    Bridge,
}

impl ProviderRealm {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Bridge => "bridge",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CompactionContext {
    pub thread_id: String,
    pub session_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct Snapshot {
    pub id: String,
    pub thread_id: String,
    pub session_id: String,
    pub request_id: String,
    pub source_model: String,
    pub parent_id: Option<String>,
    pub token_estimate: i64,
    pub payload_hash: String,
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct StoredCompaction {
    pub id: String,
    pub snapshot_id: String,
    pub realm: String,
    pub source_model: String,
    pub item: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeEnvelope {
    pub v: i64,
    pub compaction_id: String,
    pub snapshot_id: String,
    pub source_model: String,
    pub summary: String,
    pub token_estimate: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionTaskState {
    pub thread_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_id: Option<String>,
    pub model: String,
    pub realm: String,
    pub state: String,
    pub phase: String,
    #[serde(default)]
    pub journal_saved: bool,
    #[serde(default)]
    pub summary_created: bool,
    #[serde(default)]
    pub resume_verified: bool,
    #[serde(default)]
    pub original_context_retained: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default)]
    pub input_tokens_before: usize,
    #[serde(default)]
    pub summary_tokens: usize,
    #[serde(default)]
    pub chunks_completed: usize,
    #[serde(default)]
    pub chunks_total: usize,
    #[serde(default)]
    pub retry_count: usize,
    #[serde(default)]
    pub overflow_retry_count: usize,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoreCounts {
    pub snapshots: i64,
    pub compactions: i64,
    pub migrations: i64,
    pub task_states: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_defaults_to_full_switching_and_uses_stable_wire_names() {
        let settings = CompactionSettings::default();
        assert_eq!(settings.rollout_mode, CompactionRolloutMode::FullSwitching);
        assert!(settings.official_compact_fallback);
        assert_eq!(settings.summary_input_budget, 236_000);
        assert_eq!(settings.summary_max_output_tokens, 12_000);
        assert!(settings.summary_provider_id.is_none());
        assert_eq!(
            serde_json::to_string(&settings.rollout_mode).unwrap(),
            "\"full-switching\""
        );
    }

    #[test]
    fn legacy_settings_receive_safe_summary_defaults() {
        let settings: CompactionSettings = serde_json::from_value(serde_json::json!({
            "rolloutMode": "third-party-only",
            "officialCompactFallback": false
        }))
        .unwrap();
        assert_eq!(settings.summary_input_budget, DEFAULT_SUMMARY_INPUT_BUDGET);
        assert_eq!(
            settings.summary_max_output_tokens,
            DEFAULT_SUMMARY_MAX_OUTPUT_TOKENS
        );
        settings.validate().unwrap();

        // Older builds persisted this field. Unknown fields are ignored so
        // those settings remain loadable and are dropped on the next save.
        let migrated: CompactionSettings = serde_json::from_value(serde_json::json!({
            "promptProfile": "removed-profile"
        }))
        .unwrap();
        migrated.normalized().validate().unwrap();
    }

    #[test]
    fn summary_settings_trim_values_and_reject_unsafe_limits() {
        let normalized = CompactionSettings {
            summary_provider_id: Some(" provider-a ".into()),
            summary_model: Some(" model-a ".into()),
            ..Default::default()
        }
        .normalized();
        assert_eq!(
            normalized.summary_provider_id.as_deref(),
            Some("provider-a")
        );
        assert_eq!(normalized.summary_model.as_deref(), Some("model-a"));
        normalized.validate().unwrap();

        assert!(CompactionSettings {
            summary_input_budget: MIN_SUMMARY_INPUT_BUDGET - 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(CompactionSettings {
            summary_max_output_tokens: MAX_SUMMARY_MAX_OUTPUT_TOKENS + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn rollout_permissions_match_product_modes() {
        assert!(!CompactionRolloutMode::Off.captures_official());
        assert!(CompactionRolloutMode::ObserveOnly.captures_official());
        assert!(!CompactionRolloutMode::ObserveOnly.allows_bridge_compaction());
        assert!(CompactionRolloutMode::ThirdPartyOnly.allows_bridge_compaction());
        assert!(!CompactionRolloutMode::ThirdPartyOnly.allows_cross_realm());
        assert!(CompactionRolloutMode::FullSwitching.allows_cross_realm());
    }
}
