//! Async hierarchical compaction execution.
//!
//! The planner remains pure. This module owns the execution policy (chunk
//! summaries, final handoff, caching and overflow replanning) and delegates the
//! actual HTTP call to a small client trait implemented by the proxy handler.

use super::planner::{plan_summary, SummaryPlan, DEFAULT_INPUT_BUDGET};
use super::service::COMPACTION_SUMMARY_INSTRUCTIONS;
use super::store::estimate_tokens;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, future::Future, pin::Pin};

const MAX_ATTEMPTS: usize = 3;
const MIN_INPUT_BUDGET: usize = 4_000;
const FINAL_MAX_OUTPUT_TOKENS: usize = 12_000;

#[derive(Debug, Clone)]
pub(crate) struct SummaryCall {
    pub body: Value,
    pub prompt: String,
    pub max_output_tokens: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryReply {
    pub summary: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct SummaryCallError {
    pub message: String,
    pub context_overflow: bool,
}

impl SummaryCallError {
    pub(crate) fn new(message: impl Into<String>, context_overflow: bool) -> Self {
        Self {
            message: message.into(),
            context_overflow,
        }
    }
}

pub(crate) trait SummaryClient {
    fn summarize<'a>(
        &'a mut self,
        call: SummaryCall,
    ) -> Pin<Box<dyn Future<Output = Result<SummaryReply, SummaryCallError>> + Send + 'a>>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SummaryProgress {
    pub strategy: String,
    pub phase: String,
    pub chunks_completed: usize,
    pub chunks_total: usize,
    pub retries: usize,
    pub input_budget: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryExecution {
    pub summary: String,
    pub strategy: String,
    pub chunks: usize,
    pub retries: usize,
    pub overflow_retries: usize,
    pub input_tokens_before: usize,
    pub summary_tokens: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

pub(crate) async fn execute<C, P>(
    client: &mut C,
    body: &Value,
    mut on_progress: P,
) -> Result<SummaryExecution, SummaryCallError>
where
    C: SummaryClient,
    P: FnMut(SummaryProgress),
{
    let input_tokens_before = estimate_tokens(body);
    let mut cache = HashMap::<String, SummaryReply>::new();
    let mut input_budget = DEFAULT_INPUT_BUDGET;
    let mut last_error = None;

    for attempt in 0..MAX_ATTEMPTS {
        match execute_once(
            client,
            body,
            input_budget,
            attempt,
            &mut cache,
            &mut on_progress,
        )
        .await
        {
            Ok(mut execution) => {
                execution.input_tokens_before = input_tokens_before;
                execution.retries = attempt;
                execution.overflow_retries = attempt;
                execution.summary_tokens =
                    estimate_tokens(&Value::String(execution.summary.clone()));
                return Ok(execution);
            }
            Err(error) if error.context_overflow && attempt + 1 < MAX_ATTEMPTS => {
                last_error = Some(error);
                input_budget = ((input_budget as f64) * 0.70) as usize;
                input_budget = input_budget.max(MIN_INPUT_BUDGET);
                on_progress(SummaryProgress {
                    strategy: "hierarchical".to_string(),
                    phase: "replanning".to_string(),
                    retries: attempt + 1,
                    input_budget,
                    ..Default::default()
                });
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        SummaryCallError::new(
            "hierarchical compaction failed without an upstream error",
            false,
        )
    }))
}

async fn execute_once<C, P>(
    client: &mut C,
    body: &Value,
    input_budget: usize,
    retries: usize,
    cache: &mut HashMap<String, SummaryReply>,
    on_progress: &mut P,
) -> Result<SummaryExecution, SummaryCallError>
where
    C: SummaryClient,
    P: FnMut(SummaryProgress),
{
    match plan_summary(body, input_budget) {
        SummaryPlan::Single(prepared) => {
            on_progress(SummaryProgress {
                strategy: "single".to_string(),
                phase: "summarizing".to_string(),
                chunks_total: 1,
                retries,
                input_budget,
                ..Default::default()
            });
            let reply = summarize_cached(
                client,
                SummaryCall {
                    body: prepared,
                    prompt: COMPACTION_SUMMARY_INSTRUCTIONS.to_string(),
                    max_output_tokens: FINAL_MAX_OUTPUT_TOKENS,
                },
                cache,
            )
            .await?;
            on_progress(SummaryProgress {
                strategy: "single".to_string(),
                phase: "finalizing".to_string(),
                chunks_completed: 1,
                chunks_total: 1,
                retries,
                input_budget,
            });
            Ok(execution_from_replies(
                reply.summary.clone(),
                "single",
                1,
                &[reply],
            ))
        }
        SummaryPlan::Hierarchical { chunks, recent } => {
            let total = chunks.len();
            on_progress(SummaryProgress {
                strategy: "hierarchical".to_string(),
                phase: "summarizing_chunks".to_string(),
                chunks_total: total,
                retries,
                input_budget,
                ..Default::default()
            });
            let mut replies = Vec::with_capacity(total + 1);
            for (index, chunk) in chunks.into_iter().enumerate() {
                let chunk_tokens = estimate_tokens(&Value::Array(chunk.clone()));
                let max_output_tokens = ((chunk_tokens as f64 * 0.12) as usize).clamp(1_200, 4_000);
                let mut chunk_body = body.clone();
                chunk_body["input"] = Value::Array(chunk);
                let reply = summarize_cached(
                    client,
                    SummaryCall {
                        body: chunk_body,
                        prompt: chunk_prompt(index, total),
                        max_output_tokens,
                    },
                    cache,
                )
                .await?;
                replies.push(reply);
                on_progress(SummaryProgress {
                    strategy: "hierarchical".to_string(),
                    phase: "summarizing_chunks".to_string(),
                    chunks_completed: index + 1,
                    chunks_total: total,
                    retries,
                    input_budget,
                });
            }

            let mut final_input = replies
                .iter()
                .enumerate()
                .map(|(index, reply)| {
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": format!(
                                "[Earlier chronological checkpoint {}/{}]\n{}\n[End checkpoint]",
                                index + 1,
                                total,
                                reply.summary
                            )
                        }]
                    })
                })
                .collect::<Vec<_>>();
            final_input.extend(recent);
            let mut final_body = body.clone();
            final_body["input"] = Value::Array(final_input);
            on_progress(SummaryProgress {
                strategy: "hierarchical".to_string(),
                phase: "finalizing".to_string(),
                chunks_completed: total,
                chunks_total: total,
                retries,
                input_budget,
            });
            let final_reply = summarize_cached(
                client,
                SummaryCall {
                    body: final_body,
                    prompt: COMPACTION_SUMMARY_INSTRUCTIONS.to_string(),
                    max_output_tokens: FINAL_MAX_OUTPUT_TOKENS,
                },
                cache,
            )
            .await?;
            let summary = final_reply.summary.clone();
            replies.push(final_reply);
            Ok(execution_from_replies(
                summary,
                "hierarchical",
                total,
                &replies,
            ))
        }
    }
}

fn execution_from_replies(
    summary: String,
    strategy: &str,
    chunks: usize,
    replies: &[SummaryReply],
) -> SummaryExecution {
    SummaryExecution {
        summary,
        strategy: strategy.to_string(),
        chunks,
        prompt_tokens: replies.iter().map(|reply| reply.prompt_tokens).sum(),
        completion_tokens: replies.iter().map(|reply| reply.completion_tokens).sum(),
        total_tokens: replies.iter().map(|reply| reply.total_tokens).sum(),
        ..Default::default()
    }
}

async fn summarize_cached<C: SummaryClient>(
    client: &mut C,
    call: SummaryCall,
    cache: &mut HashMap<String, SummaryReply>,
) -> Result<SummaryReply, SummaryCallError> {
    let key = call_cache_key(&call);
    if let Some(reply) = cache.get(&key) {
        return Ok(reply.clone());
    }
    let reply = client.summarize(call).await?;
    if reply.summary.trim().is_empty() {
        return Err(SummaryCallError::new(
            "third-party compaction returned an empty summary",
            false,
        ));
    }
    cache.insert(key, reply.clone());
    Ok(reply)
}

fn call_cache_key(call: &SummaryCall) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&call.body).unwrap_or_default());
    digest.update(b"\0");
    digest.update(call.prompt.as_bytes());
    digest.update(b"\0");
    digest.update(call.max_output_tokens.to_le_bytes());
    URL_SAFE_DIGEST.encode(digest.finalize())
}

struct UrlSafeDigest;

impl UrlSafeDigest {
    fn encode(&self, bytes: impl AsRef<[u8]>) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }
}

const URL_SAFE_DIGEST: UrlSafeDigest = UrlSafeDigest;

fn chunk_prompt(index: usize, total: usize) -> String {
    format!(
        "{COMPACTION_SUMMARY_INSTRUCTIONS}\n\nThis is chronological older segment {} of {total}. Produce an intermediate checkpoint containing only facts needed by the final handoff. Preserve causal links between tool calls, edits, errors, and their results.",
        index + 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockClient {
        calls: usize,
        results: VecDeque<Result<SummaryReply, SummaryCallError>>,
    }

    impl SummaryClient for MockClient {
        fn summarize<'a>(
            &'a mut self,
            _call: SummaryCall,
        ) -> Pin<Box<dyn Future<Output = Result<SummaryReply, SummaryCallError>> + Send + 'a>>
        {
            self.calls += 1;
            let result = self.results.pop_front().unwrap_or_else(|| {
                Ok(SummaryReply {
                    summary: format!("summary-{}", self.calls),
                    total_tokens: 10,
                    ..Default::default()
                })
            });
            Box::pin(std::future::ready(result))
        }
    }

    #[tokio::test]
    async fn hierarchical_execution_calls_chunks_then_final() {
        let body = json!({
            "model":"test",
            "input":[
                {"type":"message","role":"user","content":"a".repeat(600_000)},
                {"type":"message","role":"user","content":"b".repeat(600_000)},
                {"type":"message","role":"user","content":"recent"}
            ]
        });
        let mut client = MockClient {
            calls: 0,
            results: VecDeque::new(),
        };
        let result = execute(&mut client, &body, |_| {}).await.expect("execute");
        assert_eq!(result.strategy, "hierarchical");
        assert!(result.chunks >= 1);
        assert_eq!(client.calls, result.chunks + 1);
        assert_eq!(result.summary, format!("summary-{}", client.calls));
    }

    #[tokio::test]
    async fn overflow_replans_and_reuses_successful_chunk_cache() {
        let body = json!({
            "model":"test",
            "input":[{"type":"message","role":"user","content":"x".repeat(1_100_000)}]
        });
        let mut client = MockClient {
            calls: 0,
            results: VecDeque::from([
                Ok(SummaryReply {
                    summary: "checkpoint".into(),
                    ..Default::default()
                }),
                Err(SummaryCallError::new("context length exceeded", true)),
            ]),
        };
        let result = execute(&mut client, &body, |_| {})
            .await
            .expect("replanned");
        assert_eq!(result.overflow_retries, 1);
        assert!(!result.summary.is_empty());
    }
}
