//! Grok CLI Proxy adapter.
//!
//! Bridges Codex (OpenAI Responses) requests to the local Grok Build
//! CLI chat proxy (`cli-chat-proxy.grok.com`). The adapter:
//!
//! - Maps the inbound Codex model to `grok-4.5`.
//! - Injects Grok auth headers (session token or API key).
//! - Injects Grok request identity headers.
//! - Sanitizes incompatible request fields using an allowlist.
//! - Delegates to `CodexAdapter` for non-Grok providers (since this
//!   adapter is the default for all `AppType::Codex` providers).

use super::adapter::auth_header_value;
use super::auth::{AuthInfo, AuthStrategy};
use super::transform_codex_chat::{
    build_codex_tool_context_from_request, CodexToolContext, CodexToolKind,
};
use super::ProviderAdapter;
use crate::provider::Provider;
use crate::proxy::error::ProxyError;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The `providerType` string stored in `ProviderMeta`.
pub const GROK_CLI_PROXY_PROVIDER_TYPE_STR: &str = "grok_cli_proxy";

/// Re-export the provider type string for `mod.rs`.
pub use GROK_CLI_PROXY_PROVIDER_TYPE_STR as GROK_CLI_PROXY_PROVIDER_TYPE;

/// Default Grok model served by the CLI chat proxy.
const DEFAULT_GROK_MODEL: &str = "grok-4.5";

/// Default base URL for the Grok CLI chat proxy.
const DEFAULT_GROK_BASE_URL: &str = "https://cli-chat-proxy.grok.com";

fn is_grok_cli_proxy_base_url(base_url: &str) -> bool {
    base_url.trim().trim_end_matches('/') == DEFAULT_GROK_BASE_URL
}

/// Reasoning effort values supported by Grok.
#[allow(dead_code)]
const SUPPORTED_EFFORTS: &[&str] = &["low", "medium", "high"];

/// Request-level allowlist: fields that are safe to forward to the
/// Grok Responses endpoint. Everything else is stripped.
const REQUEST_ALLOWLIST: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "stream",
    "max_output_tokens",
    "reasoning",
    "text",
    "metadata",
    "parallel_tool_calls",
];

/// Built-in tool types that Grok does not support. Requests containing
/// these are rejected with a clear capability error rather than silently
/// dropped.
const UNSUPPORTED_BUILTIN_TOOLS: &[&str] = &[
    "web_search",
    "file_search",
    "code_interpreter",
    "image_generation",
    "computer_use",
];

/// Thread-local cache for the credential broker. Keyed by provider id.
/// In practice there is typically only one Grok CLI proxy provider.
static BROKER_CACHE: std::sync::LazyLock<RwLock<Option<Arc<GrokBrokerEntry>>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));
static TURN_INDEXES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub struct GrokBrokerEntry {
    pub provider_id: String,
    pub broker: super::grok_credential_broker::GrokCredentialBroker,
    pub client_version: String,
    pub agent_id: Option<String>,
}

/// The Grok CLI Proxy adapter. This adapter is the default for all
/// `AppType::Codex` providers. For non-Grok providers, it delegates
/// transparently to `CodexAdapter`.
pub struct GrokCliProxyAdapter {
    codex: super::CodexAdapter,
}

impl GrokCliProxyAdapter {
    pub fn new() -> Self {
        Self {
            codex: super::CodexAdapter::new(),
        }
    }

    /// Returns `true` when the given provider is a Grok CLI proxy.
    ///
    /// Older builds could persist the preset without `meta.providerType`.
    /// Recover those providers from the exact, trusted CLI proxy endpoint so
    /// existing installations receive session-managed authentication instead
    /// of silently delegating to the generic Codex adapter.
    pub fn is_grok_provider(provider: &Provider) -> bool {
        if provider
            .meta
            .as_ref()
            .and_then(|m| m.provider_type.as_deref())
            == Some(GROK_CLI_PROXY_PROVIDER_TYPE_STR)
        {
            return true;
        }

        provider
            .settings_config
            .get("config")
            .and_then(Value::as_str)
            .and_then(crate::codex_config::extract_codex_base_url)
            .is_some_and(|base_url| is_grok_cli_proxy_base_url(&base_url))
            || provider
                .settings_config
                .get("base_url")
                .and_then(Value::as_str)
                .is_some_and(is_grok_cli_proxy_base_url)
    }

    /// Get or create the credential broker for the given provider.
    async fn get_broker(&self, provider: &Provider) -> Result<Arc<GrokBrokerEntry>, ProxyError> {
        let cache = BROKER_CACHE.read().await;
        if let Some(entry) = cache.as_ref() {
            if entry.provider_id == provider.id {
                return Ok(Arc::clone(entry));
            }
        }
        drop(cache);

        let mut cache = BROKER_CACHE.write().await;

        // Double-check after acquiring write lock.
        if let Some(entry) = cache.as_ref() {
            if entry.provider_id == provider.id {
                return Ok(Arc::clone(entry));
            }
        }

        let config = self.extract_grok_config(provider);
        let mut broker = super::grok_credential_broker::GrokCredentialBroker::new(config.clone());

        // If the provider has an API key in settings, pass it to the broker.
        if matches!(
            config.auth_mode,
            super::grok_credential_broker::GrokAuthMode::ApiKey
        ) {
            if let Some(key) = self.extract_api_key(provider) {
                broker = broker.with_api_key(key);
            }
        }

        let client_version = broker.client_version();
        let agent_id = broker.agent_id();

        let entry = Arc::new(GrokBrokerEntry {
            provider_id: provider.id.clone(),
            broker,
            client_version,
            agent_id,
        });
        *cache = Some(Arc::clone(&entry));
        Ok(entry)
    }

    /// Extract the Grok CLI proxy config from provider settings.
    /// Resolve session-managed auth and identity headers for a Grok CLI
    /// proxy provider. Returns combined auth + identity headers that the
    /// forwarder injects at request time.
    pub async fn resolve_session_headers(
        &self,
        provider: &Provider,
        session_id: &str,
    ) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, ProxyError> {
        let broker = self.get_broker(provider).await?;
        let cred = broker
            .broker
            .resolve()
            .await
            .map_err(|e| ProxyError::AuthError(format!("Grok credential error: {e}")))?;

        let mut headers = build_grok_session_auth_headers(&cred.access_token)?;

        let model = broker.broker.model().to_string();

        let identity = build_grok_identity_headers(
            &broker.client_version,
            broker.agent_id.as_deref(),
            cred.user_id.as_deref(),
            session_id,
            session_id,
            next_grok_turn_idx(session_id),
            &model,
        );
        headers.extend(identity);
        Ok(headers)
    }
    /// Public accessor for the forwarder to get broker metadata (client
    /// version, agent id) for identity headers without triggering a
    /// credential resolution.
    pub async fn get_broker_for_headers(
        &self,
        provider: &Provider,
    ) -> Result<Arc<GrokBrokerEntry>, ProxyError> {
        self.get_broker(provider).await
    }

    pub async fn refresh_session(&self, provider: &Provider) -> Result<(), ProxyError> {
        let broker = self.get_broker(provider).await?;
        broker.broker.refresh().await.map_err(|error| {
            ProxyError::AuthError(format!("Grok credential refresh failed: {error}"))
        })
    }

    fn extract_grok_config(
        &self,
        provider: &Provider,
    ) -> super::grok_credential_broker::GrokCliProxyConfig {
        let settings = &provider.settings_config;

        let auth_mode = settings
            .get("auth_mode")
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "api_key" => super::grok_credential_broker::GrokAuthMode::ApiKey,
                _ => super::grok_credential_broker::GrokAuthMode::LocalSession,
            })
            .unwrap_or_else(|| {
                if self.extract_api_key(provider).is_some() {
                    super::grok_credential_broker::GrokAuthMode::ApiKey
                } else {
                    super::grok_credential_broker::GrokAuthMode::LocalSession
                }
            });

        let model = settings
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GROK_MODEL.to_string());

        let base_url = settings
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| Some(DEFAULT_GROK_BASE_URL.to_string()));

        let api_format = settings
            .get("api_format")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| "openai_responses".to_string());

        let visible = settings
            .get("visible_in_model_picker")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        super::grok_credential_broker::GrokCliProxyConfig {
            auth_mode,
            model,
            base_url,
            grok_home: None,
            api_format,
            visible_in_model_picker: visible,
        }
    }

    /// Extract an API key from provider settings (for API key mode).
    fn extract_api_key(&self, provider: &Provider) -> Option<String> {
        let settings = &provider.settings_config;

        // Check env.XAI_API_KEY
        if let Some(env) = settings.get("env") {
            if let Some(key) = env
                .get("XAI_API_KEY")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(key.to_string());
            }
        }

        // Check auth.OPENAI_API_KEY (reusing the Codex convention)
        if let Some(auth) = settings.get("auth") {
            if let Some(key) = crate::codex_config::extract_codex_auth_api_key(auth) {
                return Some(key);
            }
        }

        // Check top-level apiKey / api_key
        settings
            .get("apiKey")
            .or_else(|| settings.get("api_key"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

impl Default for GrokCliProxyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for GrokCliProxyAdapter {
    fn name(&self) -> &'static str {
        "GrokCliProxy"
    }

    fn extract_base_url(&self, provider: &Provider) -> Result<String, ProxyError> {
        if Self::is_grok_provider(provider) {
            let config = self.extract_grok_config(provider);
            Ok(config.resolve_base_url().trim_end_matches('/').to_string())
        } else {
            // Delegate to CodexAdapter for non-Grok providers.
            self.codex.extract_base_url(provider)
        }
    }

    fn extract_auth(&self, provider: &Provider) -> Option<AuthInfo> {
        if !Self::is_grok_provider(provider) {
            return self.codex.extract_auth(provider);
        }

        // For Grok providers, the auth is resolved at request time via the
        // credential broker. The adapter returns a placeholder AuthInfo
        // with the Bearer strategy so the forwarder knows the auth shape.
        // The actual token is injected by the forwarder's Grok-specific
        // auth path (see forwarder.rs).
        let config = self.extract_grok_config(provider);
        match config.auth_mode {
            super::grok_credential_broker::GrokAuthMode::ApiKey => self
                .extract_api_key(provider)
                .map(|key| AuthInfo::new(key, AuthStrategy::Bearer)),
            super::grok_credential_broker::GrokAuthMode::LocalSession => {
                // Return a marker so the forwarder knows to use the broker.
                // The actual token will be resolved async in the forwarder.
                Some(AuthInfo::new(
                    "GROK_SESSION_MANAGED".to_string(),
                    AuthStrategy::Bearer,
                ))
            }
        }
    }

    fn build_url(&self, base_url: &str, endpoint: &str) -> String {
        // Same URL building logic as CodexAdapter.
        self.codex.build_url(base_url, endpoint)
    }

    fn get_auth_headers(
        &self,
        auth: &AuthInfo,
    ) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, ProxyError> {
        // For session-managed auth, the actual headers are injected by the
        // forwarder's Grok path. For API key mode, use standard Bearer.
        if auth.api_key == "GROK_SESSION_MANAGED" {
            // Return empty; the forwarder will inject the real headers.
            return Ok(vec![]);
        }

        let bearer = format!("Bearer {}", auth.api_key);
        Ok(vec![(
            http::HeaderName::from_static("authorization"),
            auth_header_value(&bearer)?,
        )])
    }

    fn needs_transform(&self, provider: &Provider) -> bool {
        // Grok CLI proxy uses Responses → Responses; no protocol transform.
        // Non-Grok providers delegate to CodexAdapter.
        if Self::is_grok_provider(provider) {
            true
        } else {
            self.codex.needs_transform(provider)
        }
    }

    fn transform_request(&self, mut body: Value, provider: &Provider) -> Result<Value, ProxyError> {
        if !Self::is_grok_provider(provider) {
            return self.codex.transform_request(body, provider);
        }

        // --- Grok CLI proxy request sanitization ---

        // 1. Map the model to grok-4.5 (or the provider's configured model).
        let config = self.extract_grok_config(provider);
        if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
            // Only override if the request model is a Codex model catalog entry.
            // If it's already a grok model, leave it.
            if !model.starts_with("grok") {
                body["model"] = Value::String(config.model.clone());
            }
        } else {
            body["model"] = Value::String(config.model.clone());
        }

        // 2. Strip fields not in the allowlist.
        let allowed: std::collections::HashSet<&str> = REQUEST_ALLOWLIST.iter().copied().collect();
        if let Some(obj) = body.as_object_mut() {
            let to_remove: Vec<String> = obj
                .keys()
                .filter(|key| !allowed.contains(key.as_str()))
                .cloned()
                .collect();
            for key in to_remove {
                obj.remove(&key);
            }
        }

        // 3. Translate Codex-only tool containers into Grok-compatible function tools
        // and drop hosted tools that the CLI proxy cannot execute.
        normalize_grok_tools(&mut body);

        // 4. Normalize reasoning effort.
        if let Some(reasoning) = body.get_mut("reasoning").and_then(|v| v.as_object_mut()) {
            if let Some(effort) = reasoning.get("effort").and_then(|v| v.as_str()) {
                let normalized = normalize_reasoning_effort(effort);
                reasoning.insert("effort".to_string(), Value::String(normalized.to_string()));
            }
        }

        // 5. Remove previous_response_id (Grok doesn't support it).
        if let Some(obj) = body.as_object_mut() {
            obj.remove("previous_response_id");
        }

        Ok(body)
    }
}

fn chat_function_tool_to_grok_responses_tool(tool: &Value) -> Option<Value> {
    let mut function = tool.get("function")?.as_object()?.clone();
    function.insert("type".to_string(), Value::String("function".to_string()));
    Some(Value::Object(function))
}

fn normalize_grok_input_tool_item(item: &mut Value, tool_context: &CodexToolContext) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let namespace = object
                .get("namespace")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            if namespace.is_some() {
                object.insert(
                    "name".to_string(),
                    Value::String(
                        tool_context.chat_name_for_response_function(&name, namespace.as_deref()),
                    ),
                );
                object.remove("namespace");
            }
        }
        Some("custom_tool_call") => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = object
                .remove("input")
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_default();
            object.insert(
                "type".to_string(),
                Value::String("function_call".to_string()),
            );
            object.insert("name".to_string(), Value::String(name));
            object.insert(
                "arguments".to_string(),
                Value::String(serde_json::json!({"input": input}).to_string()),
            );
        }
        Some("tool_search_call") => {
            let arguments = object
                .remove("arguments")
                .unwrap_or_else(|| serde_json::json!({}));
            object.remove("execution");
            object.insert(
                "type".to_string(),
                Value::String("function_call".to_string()),
            );
            object.insert("name".to_string(), Value::String("tool_search".to_string()));
            object.insert(
                "arguments".to_string(),
                Value::String(arguments.to_string()),
            );
        }
        Some("custom_tool_call_output") | Some("tool_search_output") => {
            let output = object
                .remove("output")
                .or_else(|| object.remove("tools"))
                .unwrap_or(Value::Null);
            object.insert(
                "type".to_string(),
                Value::String("function_call_output".to_string()),
            );
            object.insert(
                "output".to_string(),
                match output {
                    Value::String(_) => output,
                    other => Value::String(other.to_string()),
                },
            );
        }
        _ => {}
    }
}

fn normalize_grok_input_items(value: &mut Value, tool_context: &CodexToolContext) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_grok_input_items(item, tool_context);
            }
        }
        Value::Object(_) => {
            normalize_grok_input_tool_item(value, tool_context);
            if let Value::Object(object) = value {
                for child in object.values_mut() {
                    normalize_grok_input_items(child, tool_context);
                }
            }
        }
        _ => {}
    }
}

fn normalize_grok_tools(body: &mut Value) {
    let tool_context = build_codex_tool_context_from_request(body);
    if let Some(input) = body.get_mut("input") {
        normalize_grok_input_items(input, &tool_context);
    }
    let original_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut normalized_tools = Vec::new();
    let mut removed_builtin_tools = Vec::new();

    for tool in original_tools {
        let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or_default();
        if UNSUPPORTED_BUILTIN_TOOLS.contains(&tool_type) {
            removed_builtin_tools.push(tool_type.to_string());
            continue;
        }

        // These Codex tool forms are represented by the flattened function
        // definitions produced by CodexToolContext below.
        if matches!(
            tool_type,
            "function" | "namespace" | "custom" | "tool_search"
        ) {
            continue;
        }
        normalized_tools.push(tool);
    }

    normalized_tools.extend(
        tool_context
            .chat_tools()
            .iter()
            .filter_map(chat_function_tool_to_grok_responses_tool),
    );

    if !removed_builtin_tools.is_empty() {
        log::info!(
            "[GrokCliProxy] removed unsupported built-in tools: {}",
            removed_builtin_tools.join(", ")
        );
    }

    if normalized_tools.is_empty() {
        if let Some(object) = body.as_object_mut() {
            object.remove("tools");
            object.remove("tool_choice");
            object.remove("parallel_tool_calls");
        }
        return;
    }

    body["tools"] = Value::Array(normalized_tools);
    if let Some(tool_choice) = body.get("tool_choice").cloned() {
        let normalized_choice = match &tool_choice {
            Value::Object(choice)
                if choice.get("type").and_then(Value::as_str) == Some("function") =>
            {
                let name = choice
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let namespace = choice.get("namespace").and_then(Value::as_str);
                serde_json::json!({
                    "type": "function",
                    "name": tool_context.chat_name_for_response_function(name, namespace)
                })
            }
            Value::Object(choice)
                if choice.get("type").and_then(Value::as_str) == Some("custom") =>
            {
                serde_json::json!({
                    "type": "function",
                    "name": choice.get("name").and_then(Value::as_str).unwrap_or_default()
                })
            }
            Value::Object(choice)
                if choice.get("type").and_then(Value::as_str) == Some("tool_search") =>
            {
                serde_json::json!({"type": "function", "name": "tool_search"})
            }
            _ if tool_choice_targets_unsupported_builtin(&tool_choice) => {
                Value::String("auto".to_string())
            }
            _ => tool_choice,
        };
        body["tool_choice"] = normalized_choice;
    }
}

fn tool_choice_targets_unsupported_builtin(tool_choice: &Value) -> bool {
    match tool_choice {
        Value::String(choice) => UNSUPPORTED_BUILTIN_TOOLS.contains(&choice.as_str()),
        Value::Object(choice) => choice
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|tool_type| UNSUPPORTED_BUILTIN_TOOLS.contains(&tool_type)),
        _ => false,
    }
}

/// Map Codex reasoning effort to a value supported by Grok.
/// `none` → `low`, `xhigh`/`max` → `high`, others pass through.
fn normalize_reasoning_effort(effort: &str) -> &'static str {
    match effort.to_ascii_lowercase().as_str() {
        "none" => "low",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "high",
        "max" => "high",
        "ultra" => "high",
        _ => "high",
    }
}

/// Build the Grok identity headers for a request.
///
/// These headers help the Grok CLI chat proxy associate requests with
/// a stable conversation/session identity.
/// Default Grok model catalog entry for the Codex model picker.
#[allow(dead_code)]
pub fn grok_catalog_model() -> serde_json::Value {
    serde_json::json!({
        "model": DEFAULT_GROK_MODEL,
        "displayName": "Grok 4.5",
        "contextWindow": 500_000,
        "defaultReasoningLevel": "high",
        "supportedReasoningLevels": [
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
        ],
    })
}

/// Ensure a provider settings_config includes the Grok model catalog.
/// Returns updated settings with `modelCatalog.models` containing grok-4.5.
#[allow(dead_code)]
pub fn ensure_grok_model_catalog(mut settings: serde_json::Value) -> serde_json::Value {
    let catalog = settings
        .pointer_mut("/modelCatalog/models")
        .and_then(|v| v.as_array_mut());
    if let Some(models) = catalog {
        if !models
            .iter()
            .any(|m| m.get("model").and_then(|v| v.as_str()) == Some(DEFAULT_GROK_MODEL))
        {
            models.push(grok_catalog_model());
        }
    } else {
        settings["modelCatalog"] = serde_json::json!({
            "models": [grok_catalog_model()]
        });
    }
    settings
}

pub fn build_grok_identity_headers(
    client_version: &str,
    agent_id: Option<&str>,
    user_id: Option<&str>,
    conv_id: &str,
    session_id: &str,
    turn_idx: u64,
    model: &str,
) -> Vec<(http::HeaderName, http::HeaderValue)> {
    let mut headers = vec![
        ("x-grok-conv-id", conv_id.to_string()),
        ("x-grok-req-id", uuid_v4()),
        ("x-grok-model-override", model.to_string()),
        ("x-grok-session-id", session_id.to_string()),
        ("x-grok-turn-idx", turn_idx.to_string()),
        ("x-grok-client-identifier", "grok-shell".to_string()),
        ("x-grok-client-version", client_version.to_string()),
    ];

    if let Some(aid) = agent_id {
        headers.push(("x-grok-agent-id", aid.to_string()));
    }
    if let Some(uid) = user_id {
        headers.push(("x-grok-user-id", uid.to_string()));
    }

    headers
        .into_iter()
        .filter_map(|(name, value)| {
            let header_value = http::HeaderValue::from_str(&value).ok()?;
            Some((http::HeaderName::from_static(name), header_value))
        })
        .collect()
}

/// Build the Grok auth headers for a session-mode credential.
pub fn build_grok_session_auth_headers(
    access_token: &str,
) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, ProxyError> {
    Ok(vec![
        (
            http::HeaderName::from_static("authorization"),
            auth_header_value(&format!("Bearer {access_token}"))?,
        ),
        (
            http::HeaderName::from_static("x-xai-token-auth"),
            auth_header_value("xai-grok-cli")?,
        ),
    ])
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn next_grok_turn_idx(session_id: &str) -> u64 {
    let mut indexes = TURN_INDEXES
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let index = indexes.entry(session_id.to_string()).or_insert(0);
    let current = *index;
    *index = index.saturating_add(1);
    current
}

// ---------------------------------------------------------------------------
// SSE Normalizer
// ---------------------------------------------------------------------------

/// xAI event types that the Codex client does not understand. These are
/// internal xAI events (e.g. tool-echo events like `x_search`) that should
/// be filtered out before forwarding to the Codex client.
const XAI_FILTERED_EVENT_TYPES: &[&str] = &[
    "x_search",
    "x_search_call",
    "x_search_result",
    "context_details",
    "context_overflow",
];

/// State for the SSE normalizer, tracking whether lifecycle events have
/// been emitted so we can supplement any that the upstream omits.
#[derive(Default)]
struct GrokSseNormalizerState {
    /// Whether a `response.created` event has been seen.
    seen_created: bool,
    /// Whether a `response.in_progress` event has been seen.
    seen_in_progress: bool,
    /// Whether a `response.completed` or `response.failed` terminal event has been seen.
    seen_terminal: bool,
    /// Pending SSE block buffer (for multi-chunk SSE blocks).
    parse_buffer: String,
    /// Trailing bytes that form an incomplete UTF-8 sequence.
    utf8_remainder: Vec<u8>,
    /// Whether we have already emitted a synthetic `response.created`.
    synthetic_created_emitted: bool,
}

fn restore_grok_tool_call_item(item: &mut Value, tool_context: &CodexToolContext) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    if object.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let Some(chat_name) = object.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(spec) = tool_context.lookup_chat_name(chat_name).cloned() else {
        return;
    };

    match spec.kind {
        CodexToolKind::Namespace => {
            object.insert("name".to_string(), Value::String(spec.name));
            if let Some(namespace) = spec.namespace {
                object.insert("namespace".to_string(), Value::String(namespace));
            }
        }
        CodexToolKind::Custom => {
            let arguments = object
                .remove("arguments")
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_default();
            object.insert(
                "type".to_string(),
                Value::String("custom_tool_call".to_string()),
            );
            object.insert("name".to_string(), Value::String(spec.name));
            object.insert(
                "input".to_string(),
                Value::String(
                    super::transform_codex_chat::custom_tool_input_from_chat_arguments(&arguments),
                ),
            );
        }
        CodexToolKind::ToolSearch => {
            let arguments = object
                .remove("arguments")
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(&arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| serde_json::json!({"query": arguments}));
            object.remove("id");
            object.remove("name");
            object.insert(
                "type".to_string(),
                Value::String("tool_search_call".to_string()),
            );
            object.insert("execution".to_string(), Value::String("client".to_string()));
            object.insert("arguments".to_string(), parsed);
        }
        CodexToolKind::Function => {
            object.insert("name".to_string(), Value::String(spec.name));
        }
    }
}

fn restore_grok_tool_calls(value: &mut Value, tool_context: &CodexToolContext) {
    match value {
        Value::Array(items) => {
            for item in items {
                restore_grok_tool_calls(item, tool_context);
            }
        }
        Value::Object(_) => {
            restore_grok_tool_call_item(value, tool_context);
            if let Value::Object(object) = value {
                for child in object.values_mut() {
                    restore_grok_tool_calls(child, tool_context);
                }
            }
        }
        _ => {}
    }
}

fn restore_grok_tool_calls_in_sse_block(block: &str, tool_context: &CodexToolContext) -> String {
    block
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
            restore_grok_tool_calls(&mut payload, tool_context);
            format!(
                "data: {}",
                serde_json::to_string(&payload).unwrap_or_else(|_| data.to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize a single parsed SSE block. Returns the normalized bytes to
/// forward (possibly empty if the block is filtered), plus updated state.
fn normalize_sse_block_with_context(
    block: &str,
    state: &mut GrokSseNormalizerState,
    tool_context: &CodexToolContext,
) -> Vec<bytes::Bytes> {
    // Empty block (keepalive or trailing) — nothing to emit.
    if block.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();

    // Parse the event type from the block.
    let mut event_type: Option<&str> = None;
    for line in block.lines() {
        if let Some(ev) = crate::proxy::sse::strip_sse_field(line, "event") {
            event_type = Some(ev);
        }
    }

    let event_type = event_type.unwrap_or("");

    // Filter out xAI-specific events.
    if XAI_FILTERED_EVENT_TYPES.contains(&event_type) {
        // If this is a context_overflow event, we may need to emit a
        // synthetic response.failed so Codex knows the context was too long.
        if event_type == "context_overflow" && !state.seen_terminal {
            state.seen_terminal = true;
            let failed = super::codex_responses_sse::response_failed(&serde_json::json!({
                "id": format!("resp_grok_{}", uuid_v4_simple()),
                "status": "failed",
                "error": {
                    "code": "context_length_exceeded",
                    "message": "Grok context window exceeded; compaction is required",
                },
            }));
            output.push(failed);
        }
        return output;
    }

    // Track lifecycle events and supplement missing ones.
    match event_type {
        "response.created" => {
            state.seen_created = true;
        }
        "response.in_progress" => {
            state.seen_in_progress = true;
        }
        "response.completed" | "response.failed" => {
            state.seen_terminal = true;
        }
        _ => {}
    }

    // If we are about to emit output but have not yet seen a
    // `response.created`, inject a synthetic one first. The Grok proxy
    // may skip it for non-streaming responses or certain backends.
    if !state.seen_created && !state.synthetic_created_emitted && !block.is_empty() {
        // Only inject once, and only for non-keepalive events.
        if !event_type.is_empty() && event_type != "response.created" {
            state.synthetic_created_emitted = true;
            let created = super::codex_responses_sse::response_created(&serde_json::json!({
                "id": format!("resp_grok_{}", uuid_v4_simple()),
                "status": "in_progress",
                "model": DEFAULT_GROK_MODEL,
            }));
            output.push(created);
        }
    }

    // Pass the original block through unchanged.
    let restored_block = restore_grok_tool_calls_in_sse_block(block, tool_context);
    let block_bytes = format!("{}\n\n", restored_block);
    output.push(bytes::Bytes::from(block_bytes));

    output
}

#[cfg(test)]
fn normalize_sse_block(block: &str, state: &mut GrokSseNormalizerState) -> Vec<bytes::Bytes> {
    normalize_sse_block_with_context(block, state, &CodexToolContext::default())
}

/// Wrap a byte stream from the Grok upstream with an SSE normalizer that:
/// - Filters xAI-specific events (x_search, context_details, etc.)
/// - Supplements missing lifecycle events (response.created)
/// - Converts context_overflow events into response.failed
/// - Passes through all standard Responses events unchanged
#[cfg(test)]
pub fn normalize_grok_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send {
    normalize_grok_sse_stream_with_context(stream, CodexToolContext::default())
}

pub(crate) fn normalize_grok_sse_stream_with_context(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static,
    tool_context: CodexToolContext,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send {
    use futures::StreamExt;
    let stream = Box::pin(stream);

    // Use a scan to accumulate complete SSE blocks, then flatten the output.
    let mut state = GrokSseNormalizerState::default();
    let mut utf8_remainder = std::mem::take(&mut state.utf8_remainder);

    stream
        .map(move |item| {
            let mut parse_buffer = state.parse_buffer.clone();
            match item {
                Ok(chunk) => {
                    crate::proxy::sse::append_utf8_safe(
                        &mut parse_buffer,
                        &mut utf8_remainder,
                        &chunk,
                    );
                    let mut out_chunks = Vec::new();
                    while let Some(block) = crate::proxy::sse::take_sse_block(&mut parse_buffer) {
                        let mut st = GrokSseNormalizerState {
                            seen_created: state.seen_created,
                            seen_in_progress: state.seen_in_progress,
                            seen_terminal: state.seen_terminal,
                            parse_buffer: String::new(),
                            utf8_remainder: Vec::new(),
                            synthetic_created_emitted: state.synthetic_created_emitted,
                        };
                        let normalized =
                            normalize_sse_block_with_context(&block, &mut st, &tool_context);
                        state.seen_created = st.seen_created;
                        state.seen_in_progress = st.seen_in_progress;
                        state.seen_terminal = st.seen_terminal;
                        state.synthetic_created_emitted = st.synthetic_created_emitted;
                        out_chunks.extend(normalized);
                    }
                    state.parse_buffer = parse_buffer;
                    out_chunks.into_iter().map(Ok).collect::<Vec<_>>()
                }
                Err(e) => {
                    vec![Err(e)]
                }
            }
        })
        .flat_map(futures::stream::iter)
}

/// system time nanos, to keep the output deterministic in tests.
fn uuid_v4_simple() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("grok-{n:012x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderMeta;
    use serde_json::json;

    fn create_grok_provider(config: serde_json::Value) -> Provider {
        Provider {
            id: "grok-test".to_string(),
            name: "Grok Build".to_string(),
            settings_config: config,
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                provider_type: Some(GROK_CLI_PROXY_PROVIDER_TYPE_STR.to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    fn create_plain_codex_provider(config: serde_json::Value) -> Provider {
        Provider {
            id: "codex-test".to_string(),
            name: "Codex".to_string(),
            settings_config: config,
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn is_grok_provider_detects_meta_provider_type() {
        let grok = create_grok_provider(json!({}));
        let plain = create_plain_codex_provider(json!({}));
        assert!(GrokCliProxyAdapter::is_grok_provider(&grok));
        assert!(!GrokCliProxyAdapter::is_grok_provider(&plain));
    }

    #[test]
    fn is_grok_provider_recovers_legacy_preset_from_exact_cli_endpoint() {
        let legacy = create_plain_codex_provider(json!({
            "config": r#"model_provider = "custom"
model = "grok-4.5"
[model_providers.custom]
base_url = "https://cli-chat-proxy.grok.com"
wire_api = "responses"
"#
        }));
        let lookalike = create_plain_codex_provider(json!({
            "config": r#"model_provider = "custom"
model = "grok-4.5"
[model_providers.custom]
base_url = "https://cli-chat-proxy.grok.com.evil.example"
wire_api = "responses"
"#
        }));

        assert!(GrokCliProxyAdapter::is_grok_provider(&legacy));
        assert!(!GrokCliProxyAdapter::is_grok_provider(&lookalike));
    }

    #[test]
    fn extract_base_url_uses_grok_endpoint() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({
            "base_url": "https://cli-chat-proxy.grok.com"
        }));
        assert_eq!(
            adapter.extract_base_url(&provider).unwrap(),
            "https://cli-chat-proxy.grok.com"
        );
    }

    #[test]
    fn extract_base_url_delegates_to_codex_for_non_grok() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_plain_codex_provider(json!({
            "base_url": "https://api.openai.com/v1"
        }));
        assert_eq!(
            adapter.extract_base_url(&provider).unwrap(),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn transform_request_maps_model_to_grok() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({
            "model": "grok-4.5"
        }));
        let body = json!({
            "model": "gpt-5.6-sol",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "instructions": "You are a helpful assistant.",
            "stream": true
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert_eq!(result["model"], "grok-4.5");
        assert!(result.get("previous_response_id").is_none());
    }

    #[test]
    fn transform_request_strips_non_allowlisted_fields() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "previous_response_id": "resp_123",
            "service_tier": "priority",
            "store": true
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert!(result.get("previous_response_id").is_none());
        assert!(result.get("service_tier").is_none());
        assert!(result.get("store").is_none());
        assert!(result.get("model").is_some());
        assert!(result.get("input").is_some());
    }

    #[test]
    fn transform_request_drops_unsupported_builtin_tools() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "tools": [{"type": "web_search"}],
            "tool_choice": "auto"
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert!(result.get("tools").is_none());
        assert!(result.get("tool_choice").is_none());
    }

    #[test]
    fn transform_request_keeps_function_tools_when_builtin_tools_are_removed() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "tools": [
                {"type": "web_search"},
                {
                    "type": "function",
                    "name": "shell",
                    "parameters": {"type": "object", "properties": {}}
                }
            ],
            "tool_choice": {"type": "web_search"}
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert_eq!(result["tools"].as_array().unwrap().len(), 1);
        assert_eq!(result["tools"][0]["type"], "function");
        assert_eq!(result["tool_choice"], "auto");
    }

    #[test]
    fn transform_request_allows_function_tools() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "tools": [{
                "type": "function",
                "name": "shell",
                "parameters": {"type": "object", "properties": {}}
            }]
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert!(result.get("tools").is_some());
    }

    #[test]
    fn transform_request_flattens_namespace_tools_for_grok() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "tools": [{
                "type": "namespace",
                "name": "mcp__codex_apps__github",
                "tools": [{
                    "type": "function",
                    "name": "_fetch_pr",
                    "description": "Fetch a pull request",
                    "parameters": {"type": "object", "properties": {}}
                }]
            }],
            "tool_choice": {
                "type": "function",
                "namespace": "mcp__codex_apps__github",
                "name": "_fetch_pr"
            }
        });

        let result = adapter.transform_request(body, &provider).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "mcp__codex_apps__github___fetch_pr");
        assert!(tools.iter().all(|tool| tool["type"] != "namespace"));
        assert_eq!(
            result["tool_choice"]["name"],
            "mcp__codex_apps__github___fetch_pr"
        );
        assert!(result["tool_choice"].get("namespace").is_none());
    }

    #[test]
    fn transform_request_flattens_namespace_in_tool_history() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "tools": [{
                "type": "namespace",
                "name": "mcp__codex_apps__github",
                "tools": [{
                    "type": "function",
                    "name": "_fetch_pr",
                    "parameters": {"type": "object"}
                }]
            }],
            "input": [{
                "type": "function_call",
                "namespace": "mcp__codex_apps__github",
                "name": "_fetch_pr",
                "call_id": "call_1",
                "arguments": "{}"
            }]
        });

        let result = adapter.transform_request(body, &provider).unwrap();

        assert_eq!(
            result["input"][0]["name"],
            "mcp__codex_apps__github___fetch_pr"
        );
        assert!(result["input"][0].get("namespace").is_none());
    }

    #[test]
    fn restores_namespace_on_grok_function_call_items() {
        let request = json!({
            "tools": [{
                "type": "namespace",
                "name": "mcp__codex_apps__github",
                "tools": [{
                    "type": "function",
                    "name": "_fetch_pr",
                    "parameters": {"type": "object"}
                }]
            }]
        });
        let context = build_codex_tool_context_from_request(&request);
        let mut payload = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "mcp__codex_apps__github___fetch_pr",
                "call_id": "call_1",
                "arguments": "{}"
            }
        });

        restore_grok_tool_calls(&mut payload, &context);

        assert_eq!(payload["item"]["type"], "function_call");
        assert_eq!(payload["item"]["name"], "_fetch_pr");
        assert_eq!(payload["item"]["namespace"], "mcp__codex_apps__github");
    }

    #[test]
    fn transform_request_normalizes_reasoning_effort() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({}));
        let body = json!({
            "model": "grok-4.5",
            "input": [],
            "reasoning": {"effort": "xhigh"}
        });
        let result = adapter.transform_request(body, &provider).unwrap();
        assert_eq!(result["reasoning"]["effort"], "high");
    }

    #[test]
    fn normalize_effort_maps_all_values() {
        assert_eq!(normalize_reasoning_effort("none"), "low");
        assert_eq!(normalize_reasoning_effort("low"), "low");
        assert_eq!(normalize_reasoning_effort("medium"), "medium");
        assert_eq!(normalize_reasoning_effort("high"), "high");
        assert_eq!(normalize_reasoning_effort("xhigh"), "high");
        assert_eq!(normalize_reasoning_effort("max"), "high");
        assert_eq!(normalize_reasoning_effort("ultra"), "high");
        assert_eq!(normalize_reasoning_effort("unknown"), "high");
    }

    #[test]
    fn extract_auth_returns_session_managed_for_local_session() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({
            "auth_mode": "local_session"
        }));
        let auth = adapter.extract_auth(&provider).unwrap();
        assert_eq!(auth.api_key, "GROK_SESSION_MANAGED");
        assert_eq!(auth.strategy, AuthStrategy::Bearer);
    }

    #[test]
    fn extract_auth_returns_real_key_for_api_key_mode() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({
            "auth_mode": "api_key",
            "apiKey": "xai-test-key-123"
        }));
        let auth = adapter.extract_auth(&provider).unwrap();
        assert_eq!(auth.api_key, "xai-test-key-123");
    }

    #[test]
    fn configured_api_key_implicitly_selects_api_key_mode() {
        let adapter = GrokCliProxyAdapter::new();
        let provider = create_grok_provider(json!({
            "auth": {"OPENAI_API_KEY": "xai-implicit-key"}
        }));
        let auth = adapter.extract_auth(&provider).unwrap();
        assert_eq!(auth.api_key, "xai-implicit-key");
        assert!(adapter.needs_transform(&provider));
    }

    #[test]
    fn get_auth_headers_empty_for_session_managed() {
        let adapter = GrokCliProxyAdapter::new();
        let auth = AuthInfo::new("GROK_SESSION_MANAGED".to_string(), AuthStrategy::Bearer);
        let headers = adapter.get_auth_headers(&auth).unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn get_auth_headers_bearer_for_api_key() {
        let adapter = GrokCliProxyAdapter::new();
        let auth = AuthInfo::new("xai-key".to_string(), AuthStrategy::Bearer);
        let headers = adapter.get_auth_headers(&auth).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "authorization");
        assert_eq!(headers[0].1.to_str().unwrap(), "Bearer xai-key");
    }

    #[test]
    fn build_grok_identity_headers_includes_required_fields() {
        let headers = build_grok_identity_headers(
            "0.2.111",
            Some("agent-123"),
            Some("user-456"),
            "conv-abc",
            "session-xyz",
            1,
            "grok-4.5",
        );
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"x-grok-conv-id"));
        assert!(names.contains(&"x-grok-model-override"));
        assert!(names.contains(&"x-grok-agent-id"));
        assert!(names.contains(&"x-grok-user-id"));
        assert!(names.contains(&"x-grok-client-version"));
    }

    #[test]
    fn build_session_auth_headers_includes_token_and_xai_header() {
        let headers = build_grok_session_auth_headers("test-token-abc").unwrap();
        assert_eq!(headers.len(), 2);
        let auth_header = headers.iter().find(|(n, _)| n == "authorization").unwrap();
        assert_eq!(auth_header.1.to_str().unwrap(), "Bearer test-token-abc");
        let xai_header = headers
            .iter()
            .find(|(n, _)| n == "x-xai-token-auth")
            .unwrap();
        assert_eq!(xai_header.1.to_str().unwrap(), "xai-grok-cli");
    }
}
// --- SSE Normalizer tests ---

#[test]
fn normalize_filters_xai_search_events() {
    let mut state = GrokSseNormalizerState::default();
    let block = "event: x_search\ndata: {\"query\":\"test\"}";
    let output = normalize_sse_block(block, &mut state);
    // x_search events should be filtered out — no output
    assert!(output.is_empty());
}

#[test]
fn normalize_filters_context_details_events() {
    let mut state = GrokSseNormalizerState::default();
    let block = "event: context_details\ndata: {\"tokens\":1000}";
    let output = normalize_sse_block(block, &mut state);
    assert!(output.is_empty());
}

#[test]
fn normalize_converts_context_overflow_to_response_failed() {
    let mut state = GrokSseNormalizerState::default();
    let block = "event: context_overflow\ndata: {\"exceeded\":true}";
    let output = normalize_sse_block(block, &mut state);
    assert_eq!(output.len(), 1);
    let s = String::from_utf8(output[0].to_vec()).unwrap();
    assert!(s.contains("response.failed"));
    assert!(s.contains("context_length_exceeded"));
    assert!(state.seen_terminal);
}

#[test]
fn normalize_supplements_missing_response_created() {
    let mut state = GrokSseNormalizerState::default();
    // A block that is not response.created but is a real event — should
    // get a synthetic response.created prepended.
    let block = "event: response.output_text.delta\ndata: {\"delta\":\"hello\"}";
    let output = normalize_sse_block(block, &mut state);
    assert_eq!(output.len(), 2);
    let first = String::from_utf8(output[0].to_vec()).unwrap();
    assert!(first.contains("response.created"));
    let second = String::from_utf8(output[1].to_vec()).unwrap();
    assert!(second.contains("response.output_text.delta"));
    assert!(state.synthetic_created_emitted);
}

#[test]
fn normalize_passes_through_standard_events() {
    let mut state = GrokSseNormalizerState::default();
    let block = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}";
    let output = normalize_sse_block(block, &mut state);
    assert_eq!(output.len(), 1);
    let s = String::from_utf8(output[0].to_vec()).unwrap();
    assert!(s.contains("response.created"));
    assert!(state.seen_created);
}

#[test]
fn normalize_does_not_inject_created_for_empty_blocks() {
    let mut state = GrokSseNormalizerState::default();
    let output = normalize_sse_block("", &mut state);
    assert!(output.is_empty());
    assert!(!state.synthetic_created_emitted);
}

#[test]
fn normalize_tracks_terminal_events() {
    let mut state = GrokSseNormalizerState::default();
    let block = "event: response.completed\ndata: {\"type\":\"response.completed\"}";
    let output = normalize_sse_block(block, &mut state);
    assert!(!output.is_empty());
    assert!(state.seen_terminal);
}

#[tokio::test]
async fn normalize_grok_sse_stream_filters_and_passes_through() {
    use futures::StreamExt;
    let input = futures::stream::iter(vec![
            Ok(bytes::Bytes::from_static(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            )),
            Ok(bytes::Bytes::from_static(
                b"event: x_search\ndata: {\"query\":\"filtered\"}\n\n",
            )),
            Ok(bytes::Bytes::from_static(
                b"event: response.output_text.delta\ndata: {\"delta\":\"hello\"}\n\n",
            )),
        ])
        .map(|r: Result<bytes::Bytes, std::io::Error>| r);

    let normalized = normalize_grok_sse_stream(input);
    let collected: Vec<_> = normalized.collect().await;

    // Should have: response.created (passed through) + response.output_text.delta
    // x_search should be filtered out
    let all_bytes: Vec<u8> = collected
        .into_iter()
        .filter_map(|r| r.ok())
        .flat_map(|b| b.to_vec())
        .collect();
    let s = String::from_utf8(all_bytes).unwrap();
    assert!(s.contains("response.created"));
    assert!(s.contains("response.output_text.delta"));
    assert!(!s.contains("x_search"));
}
