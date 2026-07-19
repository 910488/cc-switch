use serde_json::{json, Value};

use super::store::estimate_tokens;

pub(crate) const DEFAULT_INPUT_BUDGET: usize = 236_000;
pub(crate) const DEFAULT_TOOL_OUTPUT_BUDGET: usize = 12_000;
pub(crate) const DEFAULT_RECENT_BUDGET: usize = 48_000;
pub(crate) const DEFAULT_CHUNK_BUDGET: usize = 60_000;

const TRUNCATION_MARKER: &str =
    "\n...[tool output truncated for compaction; original retained in encrypted journal]...\n";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SummaryPlan {
    Single(Value),
    Hierarchical {
        chunks: Vec<Vec<Value>>,
        recent: Vec<Value>,
    },
}

pub(crate) fn plan_summary(body: &Value, input_budget: usize) -> SummaryPlan {
    let mut prepared = body.clone();
    let tool_budget = DEFAULT_TOOL_OUTPUT_BUDGET.min((input_budget * 35) / 100);
    let input = trim_large_tool_outputs(
        prepared
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        tool_budget.max(1_000),
    );
    prepared["input"] = Value::Array(input.clone());
    if estimate_tokens(&prepared) <= input_budget {
        return SummaryPlan::Single(prepared);
    }

    let units = compaction_units(input);
    let recent_budget = DEFAULT_RECENT_BUDGET
        .min((input_budget * 30) / 100)
        .max(2_000);
    let units = units
        .into_iter()
        .flat_map(|unit| split_oversized_unit(unit, recent_budget))
        .collect();
    let (older, recent) = select_recent_units(units, recent_budget);
    let chunk_budget = DEFAULT_CHUNK_BUDGET
        .min((input_budget * 55) / 100)
        .max(4_000);
    SummaryPlan::Hierarchical {
        chunks: chunk_units(older, chunk_budget),
        recent: recent.into_iter().flatten().collect(),
    }
}

pub(crate) fn prepare_single_summary_body(body: &Value) -> Value {
    match plan_summary(body, DEFAULT_INPUT_BUDGET) {
        SummaryPlan::Single(body) => body,
        SummaryPlan::Hierarchical { chunks, recent } => {
            // The full hierarchical executor consumes the same plan. Until it is
            // needed, keep the recent context and bounded chronological excerpts
            // rather than submitting an over-limit request that is guaranteed to fail.
            let mut input = Vec::new();
            let excerpt_chars = ((DEFAULT_INPUT_BUDGET.saturating_sub(DEFAULT_RECENT_BUDGET) * 4)
                / chunks.len().max(1))
            .clamp(1_000, 8_000);
            for (index, chunk) in chunks.iter().enumerate() {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!(
                            "[Older chronological segment {}/{}; full original retained in encrypted journal]\n{}",
                            index + 1,
                            chunks.len(),
                            bounded_excerpt(chunk, excerpt_chars),
                        ),
                    }],
                }));
            }
            input.extend(recent);
            let mut prepared = body.clone();
            prepared["input"] = Value::Array(input);
            prepared
        }
    }
}

pub(crate) fn bound_migrated_continuation(body: &Value) -> Value {
    if estimate_tokens(body) <= DEFAULT_INPUT_BUDGET {
        body.clone()
    } else {
        prepare_single_summary_body(body)
    }
}

fn trim_large_tool_outputs(items: Vec<Value>, max_tokens: usize) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut item| {
            let is_output = matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
            );
            if !is_output {
                return item;
            }
            let output = item
                .get("output")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    item.get("output")
                        .cloned()
                        .unwrap_or(Value::Null)
                        .to_string()
                });
            if estimate_tokens(&Value::String(output.clone())) <= max_tokens {
                return item;
            }
            let max_chars = (max_tokens * 4).max(256);
            let available = max_chars.saturating_sub(TRUNCATION_MARKER.len()).max(128);
            let head = available * 60 / 100;
            let tail = available - head;
            let truncated = format!(
                "{}{}{}",
                take_chars(&output, head, false),
                TRUNCATION_MARKER,
                take_chars(&output, tail, true)
            );
            item["output"] = Value::String(truncated);
            item
        })
        .collect()
}

fn compaction_units(items: Vec<Value>) -> Vec<Vec<Value>> {
    let mut units: Vec<Vec<Value>> = Vec::new();
    let mut in_tool_unit = false;
    for item in items {
        let tool_history = matches!(
            item.get("type").and_then(Value::as_str),
            Some(
                "function_call"
                    | "function_call_output"
                    | "custom_tool_call"
                    | "custom_tool_call_output"
                    | "local_shell_call"
                    | "tool_search_call"
                    | "tool_search_output"
                    | "web_search_call"
            )
        );
        if tool_history && in_tool_unit {
            units.last_mut().expect("tool unit exists").push(item);
        } else {
            units.push(vec![item]);
        }
        in_tool_unit = tool_history;
    }
    units
}

fn split_oversized_unit(unit: Vec<Value>, max_tokens: usize) -> Vec<Vec<Value>> {
    if estimate_tokens(&Value::Array(unit.clone())) <= max_tokens || unit.len() != 1 {
        return vec![unit];
    }
    let item = &unit[0];
    let is_message =
        item.get("type").and_then(Value::as_str) == Some("message") || item.get("role").is_some();
    if !is_message {
        return vec![unit];
    }
    let Some(text) = message_text(item) else {
        return vec![unit];
    };
    let max_chars = (max_tokens * 3).max(1_000);
    if text.chars().count() <= max_chars {
        return vec![unit];
    }
    let chars = text.chars().collect::<Vec<_>>();
    let count = chars.len().div_ceil(max_chars);
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    chars
        .chunks(max_chars)
        .enumerate()
        .map(|(index, chunk)| {
            let mut segment = item.clone();
            segment["content"] = json!([{
                "type": content_type,
                "text": format!(
                    "[Long message segment {}/{}]\n{}",
                    index + 1,
                    count,
                    chunk.iter().collect::<String>()
                )
            }]);
            vec![segment]
        })
        .collect()
}

fn message_text(item: &Value) -> Option<String> {
    match item.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn select_recent_units(
    units: Vec<Vec<Value>>,
    max_tokens: usize,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let mut split = units.len();
    let mut tokens = 0;
    while split > 0 {
        let candidate = estimate_tokens(&Value::Array(units[split - 1].clone()));
        if split < units.len() && tokens + candidate > max_tokens {
            break;
        }
        tokens += candidate;
        split -= 1;
    }
    (units[..split].to_vec(), units[split..].to_vec())
}

fn chunk_units(units: Vec<Vec<Value>>, max_tokens: usize) -> Vec<Vec<Value>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut tokens = 0;
    for unit in units {
        let unit_tokens = estimate_tokens(&Value::Array(unit.clone()));
        if !current.is_empty() && tokens + unit_tokens > max_tokens {
            chunks.push(std::mem::take(&mut current));
            tokens = 0;
        }
        current.extend(unit);
        tokens += unit_tokens;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn bounded_excerpt(items: &[Value], max_chars: usize) -> String {
    let text = serde_json::to_string(items).unwrap_or_default();
    if text.chars().count() <= max_chars {
        text
    } else {
        format!(
            "{}{}{}",
            take_chars(&text, max_chars * 60 / 100, false),
            TRUNCATION_MARKER,
            take_chars(&text, max_chars * 40 / 100, true)
        )
    }
}

fn take_chars(text: &str, count: usize, from_end: bool) -> String {
    if from_end {
        let mut chars: Vec<char> = text.chars().rev().take(count).collect();
        chars.reverse();
        chars.into_iter().collect()
    } else {
        text.chars().take(count).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_outputs_are_middle_truncated_but_original_body_is_untouched() {
        let secret_tail = "TAIL_MARKER";
        let output = format!("HEAD_MARKER{}{}", "x".repeat(20_000), secret_tail);
        let body = json!({"input":[{"type":"function_call_output","output":output}]});
        let planned = plan_summary(&body, 8_000);
        let SummaryPlan::Single(prepared) = planned else {
            panic!("trimmed body should fit");
        };
        let trimmed = prepared["input"][0]["output"].as_str().unwrap();
        assert!(trimmed.contains("HEAD_MARKER"));
        assert!(trimmed.contains(secret_tail));
        assert!(trimmed.contains("original retained in encrypted journal"));
        assert!(!body["input"][0]["output"]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }

    #[test]
    fn hierarchy_keeps_tool_call_and_output_in_same_chunk() {
        let body = json!({"input":[
            {"type":"message","role":"user","content":"a".repeat(20_000)},
            {"type":"function_call","call_id":"c1","name":"tool","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":"small"},
            {"type":"message","role":"user","content":"recent"}
        ]});
        let SummaryPlan::Hierarchical { chunks, recent } = plan_summary(&body, 4_000) else {
            panic!("oversized body should use hierarchy");
        };
        let together_in_chunk = chunks.iter().any(|chunk| {
            chunk.iter().any(|item| item["type"] == "function_call")
                && chunk
                    .iter()
                    .any(|item| item["type"] == "function_call_output")
        });
        let together_in_recent = recent.iter().any(|item| item["type"] == "function_call")
            && recent
                .iter()
                .any(|item| item["type"] == "function_call_output");
        assert!(together_in_chunk || together_in_recent);
        assert_eq!(recent.last().unwrap()["content"], "recent");
    }

    #[test]
    fn migrated_continuation_is_unchanged_when_small_and_bounded_when_huge() {
        let small = json!({"input":[{"type":"message","content":"keep exact"}]});
        assert_eq!(bound_migrated_continuation(&small), small);
        let huge = json!({"input":[
            {"type":"message","role":"user","content":"x".repeat(DEFAULT_INPUT_BUDGET * 5)},
            {"type":"message","role":"user","content":"RECENT_SUFFIX"}
        ]});
        let bounded = bound_migrated_continuation(&huge);
        assert!(estimate_tokens(&bounded) <= DEFAULT_INPUT_BUDGET);
        assert!(bounded.to_string().contains("RECENT_SUFFIX"));
        assert!(bounded.to_string().contains("encrypted journal"));
    }

    #[test]
    fn oversized_message_is_split_before_chunking() {
        let body = json!({"input":[
            {"type":"message","role":"user","content":"x".repeat(80_000)},
            {"type":"message","role":"user","content":"recent"}
        ]});
        let SummaryPlan::Hierarchical { chunks, recent } = plan_summary(&body, 4_000) else {
            panic!("expected hierarchy");
        };
        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|chunk| estimate_tokens(&Value::Array(chunk.clone())) <= 4_000));
        assert!(recent.to_string().contains("recent"));
    }
}
