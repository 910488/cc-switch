use super::codex_chat_common::{is_empty_value, response_item_call_id};
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

const MAX_CACHED_RESPONSES: usize = 512;
const MAX_HISTORY_CHAIN_DEPTH: usize = MAX_CACHED_RESPONSES;

#[derive(Debug, Clone, Default)]
struct CachedResponse {
    calls_by_id: HashMap<String, Value>,
    call_order: Vec<String>,
    previous_response_id: Option<String>,
    input_items: Vec<Value>,
    output_items: Vec<Value>,
}

#[derive(Debug, Default)]
struct CodexChatHistoryInner {
    responses: HashMap<String, CachedResponse>,
    response_order: VecDeque<String>,
    call_index: HashMap<String, VecDeque<String>>,
}

#[derive(Debug, Clone, Default)]
struct CachedLookup {
    previous: Option<CachedResponse>,
    fallback: CachedResponse,
    history_items: Vec<Value>,
}

/// Cross-request history needed when Codex Responses is bridged to Chat
/// Completions.
///
/// Codex commonly sends follow-up turns as only `previous_response_id` plus
/// the newest input. Responses upstreams resolve that cursor server-side, but
/// Chat Completions upstreams are stateless. This store reconstructs the prior
/// user/assistant/tool turns before converting the request to Chat messages.
/// Chat providers such as DeepSeek also require an assistant message with the
/// original tool call and its `reasoning_content` immediately before the tool
/// result, so tool-call metadata remains indexed separately. Some Codex flows
/// such as subagents may omit or rewrite `previous_response_id`, so the store
/// can also fall back to a uniquely cached `call_id`.
#[derive(Debug, Default)]
pub struct CodexChatHistoryStore {
    inner: RwLock<CodexChatHistoryInner>,
}

impl CodexChatHistoryStore {
    #[cfg(test)]
    pub async fn record_response(&self, response: &Value) -> usize {
        self.record_exchange(&Value::Null, response).await
    }

    pub async fn record_exchange(&self, request: &Value, response: &Value) -> usize {
        let Some(response_id) = response
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        else {
            return 0;
        };

        let output_items = response
            .get("output")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let calls = output_items
            .iter()
            .filter_map(cached_call_item)
            .collect::<Vec<_>>();
        let previous_response_id = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let input_items = request_input_items(request);

        let mut inner = self.inner.write().await;
        inner.insert_exchange(
            response_id,
            previous_response_id,
            input_items,
            output_items,
            calls,
        )
    }

    async fn record_call_item(&self, response_id: Option<&str>, item: &Value) -> bool {
        let Some(call) = cached_call_item(item) else {
            return false;
        };

        let mut inner = self.inner.write().await;
        if let Some(response_id) = response_id.filter(|value| !value.is_empty()) {
            inner.insert_calls(response_id, vec![call]) > 0
        } else {
            false
        }
    }

    pub async fn enrich_request(&self, body: &mut Value) -> usize {
        let previous_response_id = body
            .get("previous_response_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let Some(input) = body.get_mut("input") else {
            return 0;
        };

        let original_input = std::mem::take(input);
        let original_was_object = matches!(&original_input, Value::Object(_));
        let original_string = original_input.as_str().map(ToString::to_string);
        let items = input_value_items(original_input);

        let output_call_ids = items
            .iter()
            .filter(|item| {
                item.get("type")
                    .and_then(|value| value.as_str())
                    .is_some_and(is_call_output_item_type)
            })
            .filter_map(response_item_call_id)
            .collect::<HashSet<_>>();
        let current_call_ids = items
            .iter()
            .filter(|item| {
                item.get("type")
                    .and_then(|value| value.as_str())
                    .is_some_and(is_call_item_type)
            })
            .filter_map(response_item_call_id)
            .collect::<HashSet<_>>();
        let requested_call_ids = output_call_ids
            .union(&current_call_ids)
            .cloned()
            .collect::<HashSet<_>>();
        let lookup = self
            .lookup(previous_response_id.as_deref(), &requested_call_ids)
            .await;

        let history_items = history_items_missing_from_input(&lookup.history_items, &items);
        let existing_call_ids = history_items
            .iter()
            .chain(items.iter())
            .filter(|item| {
                item.get("type")
                    .and_then(|value| value.as_str())
                    .is_some_and(is_call_item_type)
            })
            .filter_map(response_item_call_id)
            .collect::<HashSet<_>>();
        let restore_group = lookup.restore_group(&output_call_ids, &existing_call_ids);

        let restore_group_ids = restore_group
            .iter()
            .map(|(call_id, _)| call_id.clone())
            .collect::<HashSet<_>>();
        let mut restore_group = Some(restore_group);
        let mut seen_call_ids = existing_call_ids;
        let mut restored = history_items.len();
        let mut enriched = 0usize;
        let mut new_items = history_items;

        for mut item in items {
            match item.get("type").and_then(|value| value.as_str()) {
                Some(item_type) if is_call_item_type(item_type) => {
                    if let Some(call_id) = response_item_call_id(&item) {
                        if let Some(cached) = lookup.call(&call_id) {
                            if enrich_call_item_from_cache(&mut item, cached) {
                                enriched += 1;
                            }
                        }
                        seen_call_ids.insert(call_id);
                    }
                    new_items.push(item);
                }
                Some(item_type) if is_call_output_item_type(item_type) => {
                    if let Some(group) = restore_group.take().filter(|group| !group.is_empty()) {
                        for (call_id, cached_item) in group {
                            seen_call_ids.insert(call_id);
                            new_items.push(cached_item);
                            restored += 1;
                        }
                    }

                    if let Some(call_id) = response_item_call_id(&item) {
                        if !seen_call_ids.contains(&call_id)
                            && !restore_group_ids.contains(&call_id)
                        {
                            if let Some(cached) = lookup.call(&call_id).cloned() {
                                seen_call_ids.insert(call_id);
                                new_items.push(cached);
                                restored += 1;
                            }
                        }
                    }
                    new_items.push(item);
                }
                _ => new_items.push(item),
            }
        }

        let changed = restored + enriched;
        if changed == 0 && original_was_object && new_items.len() == 1 {
            *input = new_items.into_iter().next().unwrap_or(Value::Null);
        } else if changed == 0 && original_string.is_some() && new_items.len() == 1 {
            *input = Value::String(original_string.unwrap_or_default());
        } else {
            *input = Value::Array(new_items);
        }
        changed
    }

    async fn lookup(
        &self,
        previous_response_id: Option<&str>,
        requested_call_ids: &HashSet<String>,
    ) -> CachedLookup {
        let inner = self.inner.read().await;
        let previous = previous_response_id.and_then(|id| inner.responses.get(id).cloned());
        let fallback = inner.unique_fallback_calls(requested_call_ids, previous.as_ref());
        let history_items = previous_response_id
            .map(|id| inner.materialize_history(id))
            .unwrap_or_default();
        CachedLookup {
            previous,
            fallback,
            history_items,
        }
    }
}

impl CodexChatHistoryInner {
    fn insert_exchange(
        &mut self,
        response_id: &str,
        previous_response_id: Option<String>,
        input_items: Vec<Value>,
        output_items: Vec<Value>,
        calls: Vec<(String, Value)>,
    ) -> usize {
        if !self.responses.contains_key(response_id) {
            self.response_order.push_back(response_id.to_string());
        }
        self.remove_response_from_call_index(response_id);

        let mut indexed_call_ids = Vec::new();
        let inserted_or_updated = calls.len();
        {
            let cached_response = self.responses.entry(response_id.to_string()).or_default();
            cached_response.previous_response_id = previous_response_id;
            cached_response.input_items = input_items;
            cached_response.output_items = output_items;
            cached_response.calls_by_id.clear();
            cached_response.call_order.clear();
            for (call_id, item) in calls {
                cached_response.call_order.push(call_id.clone());
                cached_response.calls_by_id.insert(call_id.clone(), item);
                indexed_call_ids.push(call_id);
            }
        }
        for call_id in indexed_call_ids {
            self.index_call(&call_id, response_id);
        }

        self.prune();
        inserted_or_updated
    }

    fn insert_calls(&mut self, response_id: &str, calls: Vec<(String, Value)>) -> usize {
        if !self.responses.contains_key(response_id) {
            self.response_order.push_back(response_id.to_string());
        }

        let cached_response = self.responses.entry(response_id.to_string()).or_default();
        let mut inserted_or_updated = 0usize;
        let mut indexed_call_ids = Vec::new();
        for (call_id, item) in calls {
            if !cached_response.calls_by_id.contains_key(&call_id) {
                cached_response.call_order.push(call_id.clone());
            }
            if !cached_response
                .output_items
                .iter()
                .filter_map(response_item_call_id)
                .any(|cached_id| cached_id == call_id)
            {
                cached_response.output_items.push(item.clone());
            }
            cached_response.calls_by_id.insert(call_id.clone(), item);
            indexed_call_ids.push(call_id);
            inserted_or_updated += 1;
        }
        for call_id in indexed_call_ids {
            self.index_call(&call_id, response_id);
        }

        self.prune();
        inserted_or_updated
    }

    fn materialize_history(&self, response_id: &str) -> Vec<Value> {
        let mut chain = Vec::new();
        let mut current = Some(response_id);
        let mut seen = HashSet::new();

        while let Some(id) = current {
            if chain.len() >= MAX_HISTORY_CHAIN_DEPTH || !seen.insert(id.to_string()) {
                break;
            }
            let Some(response) = self.responses.get(id) else {
                break;
            };
            chain.push(response);
            current = response.previous_response_id.as_deref();
        }

        let mut items = Vec::new();
        for response in chain.into_iter().rev() {
            append_items_without_duplicate_prefix(&mut items, &response.input_items);
            append_items_without_duplicate_prefix(&mut items, &response.output_items);
        }
        items
    }

    fn prune(&mut self) {
        while self.response_order.len() > MAX_CACHED_RESPONSES {
            let Some(response_id) = self.response_order.pop_front() else {
                break;
            };
            self.responses.remove(&response_id);
            self.remove_response_from_call_index(&response_id);
        }
    }

    fn index_call(&mut self, call_id: &str, response_id: &str) {
        let response_ids = self.call_index.entry(call_id.to_string()).or_default();
        if !response_ids
            .iter()
            .any(|cached_id| cached_id == response_id)
        {
            response_ids.push_back(response_id.to_string());
        }
    }

    fn remove_response_from_call_index(&mut self, response_id: &str) {
        for response_ids in self.call_index.values_mut() {
            response_ids.retain(|cached_id| cached_id != response_id);
        }
        self.call_index
            .retain(|_, response_ids| !response_ids.is_empty());
    }

    fn unique_fallback_calls(
        &self,
        requested_call_ids: &HashSet<String>,
        previous: Option<&CachedResponse>,
    ) -> CachedResponse {
        let mut selected = HashMap::new();
        for call_id in requested_call_ids {
            if previous.is_some_and(|response| response.calls_by_id.contains_key(call_id)) {
                continue;
            }
            if let Some(item) = self.unique_call(call_id) {
                selected.insert(call_id.clone(), item.clone());
            }
        }

        let mut fallback = CachedResponse::default();
        for response_id in &self.response_order {
            let Some(response) = self.responses.get(response_id) else {
                continue;
            };
            for call_id in &response.call_order {
                if let Some(item) = selected.remove(call_id) {
                    fallback.call_order.push(call_id.clone());
                    fallback.calls_by_id.insert(call_id.clone(), item);
                }
            }
        }
        fallback
    }

    fn unique_call(&self, call_id: &str) -> Option<&Value> {
        let response_ids = self.call_index.get(call_id)?;
        let mut found = None;
        for response_id in response_ids {
            let Some(item) = self
                .responses
                .get(response_id)
                .and_then(|response| response.calls_by_id.get(call_id))
            else {
                continue;
            };
            if found.is_some() {
                return None;
            }
            found = Some(item);
        }
        found
    }
}

impl CachedLookup {
    fn call(&self, call_id: &str) -> Option<&Value> {
        self.previous
            .as_ref()
            .and_then(|previous| previous.calls_by_id.get(call_id))
            .or_else(|| self.fallback.calls_by_id.get(call_id))
    }

    fn restore_group(
        &self,
        output_call_ids: &HashSet<String>,
        existing_call_ids: &HashSet<String>,
    ) -> Vec<(String, Value)> {
        let mut group = Vec::new();
        let mut grouped_call_ids = HashSet::new();
        if let Some(previous) = &self.previous {
            append_restore_group(
                previous,
                output_call_ids,
                existing_call_ids,
                &mut grouped_call_ids,
                &mut group,
            );
        }
        append_restore_group(
            &self.fallback,
            output_call_ids,
            existing_call_ids,
            &mut grouped_call_ids,
            &mut group,
        );
        group
    }
}

fn append_restore_group(
    response: &CachedResponse,
    output_call_ids: &HashSet<String>,
    existing_call_ids: &HashSet<String>,
    grouped_call_ids: &mut HashSet<String>,
    group: &mut Vec<(String, Value)>,
) {
    for call_id in &response.call_order {
        if !output_call_ids.contains(call_id)
            || existing_call_ids.contains(call_id)
            || grouped_call_ids.contains(call_id)
        {
            continue;
        }
        if let Some(item) = response.calls_by_id.get(call_id).cloned() {
            grouped_call_ids.insert(call_id.clone());
            group.push((call_id.clone(), item));
        }
    }
}

fn request_input_items(request: &Value) -> Vec<Value> {
    request
        .get("input")
        .cloned()
        .map(input_value_items)
        .unwrap_or_default()
}

fn input_value_items(input: Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items,
        Value::Object(object) => vec![Value::Object(object)],
        Value::String(text) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": text,
            }],
        })],
        _ => Vec::new(),
    }
}

fn history_items_missing_from_input(history: &[Value], input: &[Value]) -> Vec<Value> {
    if history.is_empty() || (input.len() >= history.len() && input[..history.len()] == *history) {
        return Vec::new();
    }

    history
        .iter()
        .filter(|historical| {
            !input
                .iter()
                .any(|current| same_conversation_item(historical, current))
        })
        .cloned()
        .collect()
}

fn same_conversation_item(left: &Value, right: &Value) -> bool {
    let left_type = left.get("type").and_then(Value::as_str);
    let right_type = right.get("type").and_then(Value::as_str);
    if left_type.is_some_and(is_call_item_type) && right_type.is_some_and(is_call_item_type) {
        return response_item_call_id(left).is_some_and(|left_id| {
            response_item_call_id(right).is_some_and(|right_id| left_id == right_id)
        });
    }

    left.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_some_and(|left_id| right.get("id").and_then(Value::as_str) == Some(left_id))
}

fn append_items_without_duplicate_prefix(target: &mut Vec<Value>, additions: &[Value]) {
    if additions.is_empty() {
        return;
    }
    if !target.is_empty()
        && additions.len() >= target.len()
        && additions[..target.len()] == target[..]
    {
        *target = additions.to_vec();
    } else {
        target.extend_from_slice(additions);
    }
}

pub fn record_responses_sse_stream(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    history: Arc<CodexChatHistoryStore>,
    request: Value,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder = Vec::new();
        let mut current_response_id: Option<String> = None;

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                    while let Some(block) = take_sse_block(&mut buffer) {
                        inspect_sse_block(
                            &block,
                            &mut current_response_id,
                            history.as_ref(),
                            &request,
                        )
                        .await;
                    }
                    yield Ok(bytes);
                }
                Err(err) => yield Err(err),
            }
        }
    }
}

async fn inspect_sse_block(
    block: &str,
    current_response_id: &mut Option<String>,
    history: &CodexChatHistoryStore,
    request: &Value,
) {
    if block.trim().is_empty() {
        return;
    }

    let mut data_parts = Vec::new();
    for line in block.lines() {
        if let Some(data) = strip_sse_field(line, "data") {
            data_parts.push(data.to_string());
        }
    }

    let data = data_parts.join("\n");
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return;
    }

    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return;
    };

    if let Some(response_id) = value
        .pointer("/response/id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
    {
        *current_response_id = Some(response_id.to_string());
    }

    match value.get("type").and_then(|value| value.as_str()) {
        Some("response.output_item.done") => {
            if let Some(item) = value.get("item") {
                history
                    .record_call_item(current_response_id.as_deref(), item)
                    .await;
            }
        }
        Some("response.completed") => {
            if let Some(response) = value.get("response") {
                history.record_exchange(request, response).await;
            }
        }
        _ => {}
    }
}

fn cached_call_item(item: &Value) -> Option<(String, Value)> {
    if !item
        .get("type")
        .and_then(|value| value.as_str())
        .is_some_and(is_call_item_type)
    {
        return None;
    }
    let call_id = response_item_call_id(item)?;
    Some((call_id, item.clone()))
}

fn is_call_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_search_call"
    )
}

fn is_call_output_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "function_call_output" | "custom_tool_call_output" | "tool_search_output"
    )
}

fn enrich_call_item_from_cache(item: &mut Value, cached: &Value) -> bool {
    let mut changed = false;
    for key in [
        "name",
        "namespace",
        "arguments",
        "input",
        "status",
        "execution",
        "reasoning_content",
        "reasoning",
    ] {
        if item.get(key).is_some_and(|value| !is_empty_value(value)) {
            continue;
        }
        let Some(value) = cached.get(key).filter(|value| !is_empty_value(value)) else {
            continue;
        };
        if let Some(object) = item.as_object_mut() {
            object.insert(key.to_string(), value.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[tokio::test]
    async fn enriches_tool_output_with_cached_function_call_from_previous_response() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}",
                        "reasoning_content": "Need to inspect the file."
                    }
                ]
            }))
            .await;

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["reasoning_content"], "Need to inspect the file.");
        assert_eq!(input[1]["type"], "function_call_output");
    }

    #[tokio::test]
    async fn restores_unique_call_id_without_matching_previous_response() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "read_file",
                        "arguments": "{}",
                        "reasoning_content": "This is the only cached call."
                    }
                ]
            }))
            .await;

        let mut missing_previous = json!({
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });
        assert_eq!(history.enrich_request(&mut missing_previous).await, 1);
        assert_eq!(missing_previous["input"][0]["type"], "function_call");
        assert_eq!(
            missing_previous["input"][0]["reasoning_content"],
            "This is the only cached call."
        );
        assert_eq!(missing_previous["input"][1]["type"], "function_call_output");

        let mut different_previous = json!({
            "previous_response_id": "resp_2",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });
        assert_eq!(history.enrich_request(&mut different_previous).await, 1);
        assert_eq!(different_previous["input"][0]["type"], "function_call");
        assert_eq!(
            different_previous["input"][0]["reasoning_content"],
            "This is the only cached call."
        );
        assert_eq!(
            different_previous["input"][1]["type"],
            "function_call_output"
        );
    }

    #[tokio::test]
    async fn does_not_restore_ambiguous_call_id_without_previous_response() {
        let history = CodexChatHistoryStore::default();
        for (response_id, reasoning) in [
            ("resp_1", "This belongs to the first response."),
            ("resp_2", "This belongs to the second response."),
        ] {
            history
                .record_response(&json!({
                    "id": response_id,
                    "output": [
                        {
                            "type": "function_call",
                            "call_id": "call_1",
                            "name": "read_file",
                            "arguments": "{}",
                            "reasoning_content": reasoning
                        }
                    ]
                }))
                .await;
        }

        let mut missing_previous = json!({
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });
        assert_eq!(history.enrich_request(&mut missing_previous).await, 0);
        assert_eq!(missing_previous["input"][0]["type"], "function_call_output");

        let mut different_previous = json!({
            "previous_response_id": "resp_missing",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });
        assert_eq!(history.enrich_request(&mut different_previous).await, 0);
        assert_eq!(
            different_previous["input"][0]["type"],
            "function_call_output"
        );
    }

    #[tokio::test]
    async fn enriches_existing_function_call_missing_reasoning() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "read_file",
                        "arguments": "{}",
                        "reasoning_content": "Need to inspect the file."
                    }
                ]
            }))
            .await;

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["reasoning_content"], "Need to inspect the file.");
        assert_eq!(input.len(), 2);
    }

    #[tokio::test]
    async fn enriches_existing_function_call_missing_name_and_arguments() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}",
                        "reasoning_content": "Need to inspect the file."
                    }
                ]
            }))
            .await;

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], "read_file");
        assert_eq!(input[0]["arguments"], "{\"path\":\"README.md\"}");
        assert_eq!(input[0]["reasoning_content"], "Need to inspect the file.");
        assert_eq!(input[1]["type"], "function_call_output");
    }

    #[tokio::test]
    async fn restores_parallel_tool_calls_as_one_assistant_group() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}",
                        "reasoning_content": "Need both tools."
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_2",
                        "name": "second",
                        "arguments": "{}",
                        "reasoning_content": "Need both tools."
                    }
                ]
            }))
            .await;

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "one"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_2",
                    "output": "two"
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 2);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_2");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[tokio::test]
    async fn restores_custom_and_tool_search_calls_from_previous_response() {
        let history = CodexChatHistoryStore::default();
        history
            .record_response(&json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_patch",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** End Patch",
                        "reasoning_content": "Need to patch the file."
                    },
                    {
                        "type": "tool_search_call",
                        "call_id": "call_search",
                        "status": "completed",
                        "execution": "client",
                        "arguments": {"query": "Gmail tools"},
                        "reasoning_content": "Need to discover tools."
                    }
                ]
            }))
            .await;

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_patch",
                    "output": "patched"
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_search",
                    "tools": []
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 2);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "custom_tool_call");
        assert_eq!(input[0]["call_id"], "call_patch");
        assert_eq!(input[1]["type"], "tool_search_call");
        assert_eq!(input[1]["call_id"], "call_search");
        assert_eq!(input[2]["type"], "custom_tool_call_output");
        assert_eq!(input[3]["type"], "tool_search_output");
    }

    #[tokio::test]
    async fn restores_regular_user_and_assistant_history_from_previous_response() {
        let history = CodexChatHistoryStore::default();
        let first_request = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text","text":"Discuss Taiwan stock data sources"}]
            }]
        });
        history
            .record_exchange(
                &first_request,
                &json!({
                    "id": "resp_1",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type":"output_text","text":"Compare Fugle and broker APIs."}]
                    }]
                }),
            )
            .await;

        let mut second_request = json!({
            "previous_response_id": "resp_1",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text","text":"Search those sources carefully"}]
            }]
        });

        assert_eq!(history.enrich_request(&mut second_request).await, 2);
        let input = second_request["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(
            input[1]["content"][0]["text"],
            "Compare Fugle and broker APIs."
        );

        let chat =
            super::super::transform_codex_chat::responses_to_chat_completions(second_request)
                .unwrap();
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
    }

    #[tokio::test]
    async fn restores_cumulative_history_across_multiple_stateless_chat_turns() {
        let history = CodexChatHistoryStore::default();
        let first_request = json!({
            "input": [{"type":"message","role":"user","content":"first question"}]
        });
        history
            .record_exchange(
                &first_request,
                &json!({
                    "id":"resp_1",
                    "output":[{"type":"message","role":"assistant","content":"first answer"}]
                }),
            )
            .await;

        let second_original = json!({
            "previous_response_id":"resp_1",
            "input":[{"type":"message","role":"user","content":"second question"}]
        });
        history
            .record_exchange(
                &second_original,
                &json!({
                    "id":"resp_2",
                    "output":[{"type":"message","role":"assistant","content":"second answer"}]
                }),
            )
            .await;

        let mut third_request = json!({
            "previous_response_id":"resp_2",
            "input":[{"type":"message","role":"user","content":"what did we discuss?"}]
        });
        assert_eq!(history.enrich_request(&mut third_request).await, 4);
        let input = third_request["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["content"], "first question");
        assert_eq!(input[1]["content"], "first answer");
        assert_eq!(input[2]["content"], "second question");
        assert_eq!(input[3]["content"], "second answer");
        assert_eq!(input[4]["content"], "what did we discuss?");

        let mut already_expanded = third_request.clone();
        assert_eq!(history.enrich_request(&mut already_expanded).await, 0);
        assert_eq!(already_expanded["input"], third_request["input"]);
    }

    #[tokio::test]
    async fn streamed_text_response_restores_context_on_the_next_turn() {
        let history = Arc::new(CodexChatHistoryStore::default());
        let first_request = json!({
            "input":[{"type":"message","role":"user","content":"initial topic"}]
        });
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
            br#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_stream_text","output":[{"type":"message","role":"assistant","content":"initial answer"}]}}

"#,
        ))]);

        let output = record_responses_sse_stream(stream, history.clone(), first_request)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(output.len(), 1);

        let mut next_request = json!({
            "previous_response_id":"resp_stream_text",
            "input":[{"type":"message","role":"user","content":"follow-up"}]
        });
        assert_eq!(history.enrich_request(&mut next_request).await, 2);
        let input = next_request["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"], "initial topic");
        assert_eq!(input[1]["content"], "initial answer");
        assert_eq!(input[2]["content"], "follow-up");
    }

    #[tokio::test]
    async fn records_streamed_function_call_done_items() {
        let history = Arc::new(CodexChatHistoryStore::default());
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\"}}\n\n",
            )),
            Ok(Bytes::from_static(
                b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{}\",\"reasoning_content\":\"Need a file.\"}}\n\n",
            )),
        ]);

        let output = record_responses_sse_stream(stream, history.clone(), Value::Null)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(output.len(), 2);

        let mut request = json!({
            "previous_response_id": "resp_stream",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        assert_eq!(request["input"][0]["reasoning_content"], "Need a file.");
    }
}
