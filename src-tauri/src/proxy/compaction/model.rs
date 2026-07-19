use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const ENVELOPE_PREFIX: &str = "bcmp1.";
pub(crate) const ENVELOPE_VERSION: i64 = 1;
pub(crate) const KEY_VERSION: i64 = 1;
pub(crate) const MIGRATION_PROMPT_VERSION: i64 = 1;

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
pub(crate) struct CompactionTaskState {
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
