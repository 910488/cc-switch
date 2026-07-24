//! Hides third-party chain-of-thought text from Codex while preserving the
//! Responses reasoning-item lifecycle and opaque continuation metadata.
//!
//! Official OpenAI responses are never passed through this module. Third-party
//! providers frequently expose raw reasoning as reasoning_summary_text, which
//! Codex Desktop renders as user-visible prose. We keep the reasoning item so
//! output indexes and tool-call history remain stable, but remove the summary
//! text and retain fields such as encrypted_content.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

fn event_type_from_block(block: &str) -> Option<String> {
    for line in block.lines() {
        if let Some(event) = crate::proxy::sse::strip_sse_field(line, "event") {
            return Some(event.trim().to_string());
        }
    }

    for line in block.lines() {
        let Some(data) = crate::proxy::sse::strip_sse_field(line, "data") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<Value>(data) {
            if let Some(event_type) = payload.get("type").and_then(Value::as_str) {
                return Some(event_type.to_string());
            }
        }
    }
    None
}

fn is_visible_reasoning_text_event(event_type: &str) -> bool {
    event_type.starts_with("response.reasoning_summary_")
        || matches!(
            event_type,
            "response.reasoning_text.delta"
                | "response.reasoning_text.done"
                | "response.reasoning.delta"
                | "response.reasoning.done"
        )
}

/// Remove visible reasoning summaries while preserving reasoning item identity,
/// status and opaque continuation fields.
pub(crate) fn hide_reasoning_summaries(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                hide_reasoning_summaries(item);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("reasoning") {
                object.insert("summary".to_string(), Value::Array(Vec::new()));
            }
            for child in object.values_mut() {
                hide_reasoning_summaries(child);
            }
        }
        _ => {}
    }
}

fn hide_reasoning_in_sse_block(block: &str) -> Option<Bytes> {
    if block.trim().is_empty() {
        return None;
    }
    if event_type_from_block(block)
        .as_deref()
        .is_some_and(is_visible_reasoning_text_event)
    {
        return None;
    }

    let normalized = block
        .lines()
        .map(|line| {
            let Some(data) = crate::proxy::sse::strip_sse_field(line, "data") else {
                return line.to_string();
            };
            if data.trim() == "[DONE]" {
                return line.to_string();
            }
            let Ok(mut payload) = serde_json::from_str::<Value>(data) else {
                return line.to_string();
            };
            hide_reasoning_summaries(&mut payload);
            format!(
                "data: {}",
                serde_json::to_string(&payload).unwrap_or_else(|_| data.to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Some(Bytes::from(format!("{normalized}\n\n")))
}

/// Filter visible third-party reasoning text from a Responses SSE stream.
pub(crate) fn hide_reasoning_summaries_stream<E: std::error::Error + Send + Sync + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder = Vec::new();
        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    crate::proxy::sse::append_utf8_safe(
                        &mut buffer,
                        &mut utf8_remainder,
                        &bytes,
                    );
                    while let Some(block) = crate::proxy::sse::take_sse_block(&mut buffer) {
                        if let Some(block) = hide_reasoning_in_sse_block(&block) {
                            yield Ok(block);
                        }
                    }
                }
                Err(error) => {
                    yield Err(std::io::Error::other(error.to_string()));
                    return;
                }
            }
        }

        if !utf8_remainder.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&utf8_remainder));
        }
        if !buffer.trim().is_empty() {
            if let Some(block) = hide_reasoning_in_sse_block(buffer.trim_end()) {
                yield Ok(block);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[test]
    fn response_sanitizer_keeps_opaque_reasoning_but_hides_summary() {
        let mut response = json!({
            "output": [{
                "id": "rs_1",
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "private chain of thought"}],
                "encrypted_content": "opaque-continuation"
            }]
        });

        hide_reasoning_summaries(&mut response);

        assert_eq!(response["output"][0]["summary"], json!([]));
        assert_eq!(
            response["output"][0]["encrypted_content"],
            "opaque-continuation"
        );
    }

    #[tokio::test]
    async fn stream_sanitizer_drops_reasoning_text_and_clears_completed_item() {
        let input = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}\n\nevent: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"private\"}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"private\"}]}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
        ))]);

        let output = hide_reasoning_summaries_stream(input)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .fold(Vec::new(), |mut bytes, chunk| {
                bytes.extend_from_slice(&chunk);
                bytes
            });
        let text = String::from_utf8(output).unwrap();

        assert!(!text.contains("reasoning_summary_text.delta"));
        assert!(!text.contains("private"));
        assert!(text.contains("\"summary\":[]"));
        assert!(text.contains("response.output_text.delta"));
        assert!(text.contains("answer"));
    }
}
