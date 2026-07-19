//! HTTP代理服务器
//!
//! 基于Axum的HTTP服务器，处理代理请求
//!
//! Uses a manual hyper HTTP/1.1 accept loop with `preserve_header_case(true)` so
//! that the original header-name casing from the CLI client is captured in a
//! `HeaderCaseMap` extension.  This map is later forwarded to the upstream via
//! the hyper-based HTTP client, producing wire-level header casing identical to
//! a direct (non-proxied) CLI request.

use super::{
    compaction::CompactionService,
    failover_switch::FailoverSwitchManager,
    handlers,
    log_codes::srv as log_srv,
    provider_router::ProviderRouter,
    providers::{codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore},
    types::*,
    ProxyError,
};
use crate::database::Database;
use axum::{
    extract::DefaultBodyLimit,
    routing::{any, get, post},
    Router,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

/// 代理服务器状态（共享）
#[derive(Clone)]
pub struct ProxyState {
    pub db: Arc<Database>,
    pub config: Arc<RwLock<ProxyConfig>>,
    pub status: Arc<RwLock<ProxyStatus>>,
    pub start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// 每个应用类型当前使用的 provider (app_type -> (provider_id, provider_name))
    pub current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
    /// 共享的 ProviderRouter（持有熔断器状态，跨请求保持）
    pub provider_router: Arc<ProviderRouter>,
    /// Gemini Native shadow state，用于 thoughtSignature / tool call 回放
    pub gemini_shadow: Arc<GeminiShadowStore>,
    /// Codex Chat bridge history，用于恢复 previous_response_id 指向的 tool call
    pub codex_chat_history: Arc<CodexChatHistoryStore>,
    /// Durable encrypted journal used to keep Codex context valid while providers change.
    pub compaction_service: Arc<CompactionService>,
    /// AppHandle，用于发射事件和更新托盘菜单
    pub app_handle: Option<tauri::AppHandle>,
    /// 故障转移切换管理器
    pub failover_manager: Arc<FailoverSwitchManager>,
}

/// 代理HTTP服务器
pub struct ProxyServer {
    config: ProxyConfig,
    state: ProxyState,
    shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// 服务器任务句柄，用于等待服务器实际关闭
    server_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl ProxyServer {
    pub fn new(
        config: ProxyConfig,
        db: Arc<Database>,
        app_handle: Option<tauri::AppHandle>,
    ) -> Self {
        // 创建共享的 ProviderRouter（熔断器状态将跨所有请求保持）
        let provider_router = Arc::new(ProviderRouter::new(db.clone()));
        // 创建故障转移切换管理器
        let failover_manager = Arc::new(FailoverSwitchManager::new(db.clone()));

        let compaction_service = Arc::new(CompactionService::new(db.clone()));
        let state = ProxyState {
            db,
            config: Arc::new(RwLock::new(config.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            current_providers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_router,
            gemini_shadow: Arc::new(GeminiShadowStore::default()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            compaction_service,
            app_handle,
            failover_manager,
        };

        Self {
            config,
            state,
            shutdown_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, ProxyError> {
        // 检查是否已在运行
        if self.shutdown_tx.read().await.is_some() {
            return Err(ProxyError::AlreadyRunning);
        }

        let addr: SocketAddr =
            format!("{}:{}", self.config.listen_address, self.config.listen_port)
                .parse()
                .map_err(|e| ProxyError::BindFailed(format!("无效的地址: {e}")))?;

        // 创建关闭通道
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // 构建路由
        let app = self.build_router();

        // 绑定监听器
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let actual_port = local_addr.port();

        log::info!("[{}] 代理服务器启动于 {local_addr}", log_srv::STARTED);

        // 更新全局代理端口，用于系统代理检测
        crate::proxy::http_client::set_proxy_port(actual_port);

        // 保存关闭句柄
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // 更新状态
        let mut status = self.state.status.write().await;
        status.running = true;
        status.address = self.config.listen_address.clone();
        status.port = actual_port;
        drop(status);

        // 记录启动时间
        *self.state.start_time.write().await = Some(std::time::Instant::now());

        // 启动服务器 — 使用手动 hyper HTTP/1.1 accept loop
        // 开启 preserve_header_case 以捕获客户端请求头的原始大小写
        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _remote_addr) = match result {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!("[{SRV}] accept 失败: {e}", SRV = log_srv::ACCEPT_ERR);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };

                        let app = app.clone();
                        tokio::spawn(async move {
                            // Peek raw TCP bytes to capture original header casing
                            // before hyper parses (and lowercases) the header names.
                            let original_cases = {
                                let mut peek_buf = vec![0u8; 8192];
                                match stream.peek(&mut peek_buf).await {
                                    Ok(n) => {
                                        let cases = super::hyper_client::OriginalHeaderCases::from_raw_bytes(&peek_buf[..n]);
                                        log::debug!(
                                            "[ProxyServer] Peeked {} bytes, captured {} header casings",
                                            n, cases.cases.len()
                                        );
                                        cases
                                    }
                                    Err(e) => {
                                        log::debug!("[ProxyServer] peek failed (non-fatal): {e}");
                                        super::hyper_client::OriginalHeaderCases::default()
                                    }
                                }
                            };

                            // service_fn 将 axum Router（tower::Service）桥接到 hyper
                            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                let mut router = app.clone();
                                let cases = original_cases.clone();
                                async move {
                                    // 将 hyper::body::Incoming 转为 axum::body::Body，保留 extensions
                                    let (mut parts, body) = req.into_parts();

                                    // Insert our own header case map alongside hyper's internal one
                                    parts.extensions.insert(cases);

                                    let body = axum::body::Body::new(body);
                                    let axum_req = http::Request::from_parts(parts, body);
                                    <Router as tower::Service<http::Request<axum::body::Body>>>::call(&mut router, axum_req).await
                                }
                            });

                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .preserve_header_case(true)
                                .serve_connection(TokioIo::new(stream), service)
                                .await
                            {
                                // Connection reset / broken pipe 等在代理场景下很常见，debug 级别
                                log::debug!("[{SRV}] connection error: {e}", SRV = log_srv::CONN_ERR);
                            }
                        });
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }

            // 服务器停止后更新状态
            state.status.write().await.running = false;
            *state.start_time.write().await = None;
        });

        // 保存服务器任务句柄
        *self.server_handle.write().await = Some(handle);

        Ok(ProxyServerInfo {
            address: self.config.listen_address.clone(),
            port: actual_port,
            started_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        // 1. 发送关闭信号
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        } else {
            return Err(ProxyError::NotRunning);
        }

        // 2. 等待服务器任务结束（带 5 秒超时保护）
        if let Some(handle) = self.server_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    log::info!("[{}] 代理服务器已完全停止", log_srv::STOPPED);
                    Ok(())
                }
                Ok(Err(e)) => {
                    log::warn!("[{}] 代理服务器任务异常终止: {e}", log_srv::TASK_ERROR);
                    Err(ProxyError::StopFailed(e.to_string()))
                }
                Err(_) => {
                    log::warn!(
                        "[{}] 代理服务器停止超时（5秒），强制继续",
                        log_srv::STOP_TIMEOUT
                    );
                    Err(ProxyError::StopTimeout)
                }
            }
        } else {
            Ok(())
        }
    }

    pub async fn get_status(&self) -> ProxyStatus {
        let mut status = self.state.status.read().await.clone();

        // 计算运行时间
        if let Some(start) = *self.state.start_time.read().await {
            status.uptime_seconds = start.elapsed().as_secs();
        }

        // 从 current_providers HashMap 获取每个应用类型当前正在使用的 provider
        let current_providers = self.state.current_providers.read().await;
        status.active_targets = current_providers
            .iter()
            .map(|(app_type, (provider_id, provider_name))| ActiveTarget {
                app_type: app_type.clone(),
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
            })
            .collect();
        status.continuity = self.state.compaction_service.status();

        status
    }

    /// 更新某个应用类型当前“目标供应商”（用于 UI 展示 active_targets）
    ///
    /// 注意：这不代表该供应商一定已经处理过请求，而是用于“热切换/启用故障转移立即切 P1”
    /// 等场景下，让 UI 能立刻反映最新目标。
    pub async fn set_active_target(&self, app_type: &str, provider_id: &str, provider_name: &str) {
        let mut current_providers = self.state.current_providers.write().await;
        current_providers.insert(
            app_type.to_string(),
            (provider_id.to_string(), provider_name.to_string()),
        );
    }

    fn build_router(&self) -> Router {
        Router::new()
            // 健康检查
            .route("/health", get(handlers::health_check))
            .route("/status", get(handlers::get_status))
            // Claude API (支持带前缀和不带前缀两种格式)
            .route("/v1/messages", post(handlers::handle_messages))
            .route("/claude/v1/messages", post(handlers::handle_messages))
            // Claude Desktop 3P 本地 gateway（独立 provider namespace）
            .route(
                "/claude-desktop/v1/models",
                get(handlers::handle_claude_desktop_models),
            )
            .route(
                "/claude-desktop/v1/messages",
                post(handlers::handle_claude_desktop_messages),
            )
            // OpenAI Chat Completions API (Codex CLI，支持带前缀和不带前缀)
            .route("/chat/completions", post(handlers::handle_chat_completions))
            .route(
                "/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/v1/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/codex/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            // OpenAI Models API (Codex CLI reachability check)
            .route("/models", get(handlers::handle_models))
            .route("/v1/models", get(handlers::handle_models))
            // OpenAI Responses API (Codex CLI，支持带前缀和不带前缀)
            .route("/responses", post(handlers::handle_responses))
            .route("/v1/responses", post(handlers::handle_responses))
            .route("/v1/v1/responses", post(handlers::handle_responses))
            .route("/codex/v1/responses", post(handlers::handle_responses))
            // Grok Build uses the Responses protocol but has an independent
            // provider namespace and failover queue.
            .route(
                "/grokbuild/v1/responses",
                post(handlers::handle_grokbuild_responses),
            )
            // OpenAI Responses Compact API (Codex CLI 远程压缩，透传)
            .route(
                "/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/codex/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/grokbuild/v1/responses/compact",
                post(handlers::handle_grokbuild_responses_compact),
            )
            // Gemini API (支持带前缀和不带前缀)
            //
            // 用 `any(..)` 覆盖所有 HTTP 方法：除了 POST `:generateContent` /
            // `:streamGenerateContent` / `:countTokens` 之外，Gemini SDK / CLI 还会发
            // GET `/models`、GET `/models/<id>` 等只读端点。如果只挂 POST，这些 GET
            // 请求会在路由层 404，绕过本地代理的统计、整流和故障转移。
            .route("/v1beta/*path", any(handlers::handle_gemini))
            .route("/gemini/v1beta/*path", any(handlers::handle_gemini))
            // Gemini 的 GA 版本也叫 /v1，给原 SDK 留一条出口
            .route("/gemini/v1/*path", any(handlers::handle_gemini))
            // 提高默认请求体大小限制（避免 413 Payload Too Large）
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
            .with_state(self.state.clone())
    }

    /// 在不重启服务的情况下更新运行时配置
    pub async fn apply_runtime_config(&self, config: &ProxyConfig) {
        *self.state.config.write().await = config.clone();
    }

    /// 热更新熔断器配置
    ///
    /// 将新配置应用到所有已创建的熔断器实例
    pub async fn update_circuit_breaker_configs(
        &self,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state.provider_router.update_all_configs(config).await;
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state
            .provider_router
            .update_app_configs(app_type, config)
            .await;
    }

    /// 重置指定 Provider 的熔断器
    pub async fn reset_provider_circuit_breaker(&self, provider_id: &str, app_type: &str) {
        self.state
            .provider_router
            .reset_provider_breaker(provider_id, app_type)
            .await;
    }
}

#[cfg(test)]
mod continuity_e2e_tests {
    use super::*;
    use axum::{extract::State, response::IntoResponse, routing::post, Json};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use bytes::Bytes;
    use futures::StreamExt;
    use serde_json::{json, Value};
    use serial_test::serial;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    struct TestEnvironment {
        _home: TempDir,
        old_test_home: Option<String>,
        old_key: Option<String>,
    }

    impl TestEnvironment {
        fn new() -> Self {
            let home = TempDir::new().expect("temp test home");
            let old_test_home = std::env::var("CC_SWITCH_TEST_HOME").ok();
            let old_key = std::env::var("CC_SWITCH_COMPACTION_MASTER_KEY").ok();
            std::env::set_var("CC_SWITCH_TEST_HOME", home.path());
            std::env::set_var(
                "CC_SWITCH_COMPACTION_MASTER_KEY",
                STANDARD.encode(vec![42u8; 32]),
            );
            crate::settings::reload_settings().expect("reload isolated settings");
            Self {
                _home: home,
                old_test_home,
                old_key,
            }
        }

        fn db_path(&self) -> std::path::PathBuf {
            self._home.path().join(".cc-switch").join("cc-switch.db")
        }
    }

    impl Drop for TestEnvironment {
        fn drop(&mut self) {
            match &self.old_test_home {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            match &self.old_key {
                Some(value) => std::env::set_var("CC_SWITCH_COMPACTION_MASTER_KEY", value),
                None => std::env::remove_var("CC_SWITCH_COMPACTION_MASTER_KEY"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    async fn mock_chat(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        captured.lock().expect("capture lock").push(body);
        Json(json!({
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "model": "mock-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "HANDOFF_E2E_SUMMARY: durable context retained"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28}
        }))
    }

    async fn start_mock_chat_upstream() -> (
        String,
        Arc<Mutex<Vec<Value>>>,
        oneshot::Sender<()>,
        JoinHandle<()>,
    ) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_chat))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let address = listener.local_addr().expect("mock address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("mock upstream server");
        });
        (format!("http://{address}/v1"), captured, shutdown_tx, task)
    }

    #[derive(Default)]
    struct QuotaMockState {
        a_requests: usize,
        b_requests: usize,
    }

    async fn quota_a(
        State(state): State<Arc<Mutex<QuotaMockState>>>,
        Json(_body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        state.lock().expect("quota state").a_requests += 1;
        (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": {"code": "insufficient_quota", "message": "usage limit reached"},
                "retry_after": 600
            })),
        )
    }

    async fn quota_b(
        State(state): State<Arc<Mutex<QuotaMockState>>>,
        Json(_body): Json<Value>,
    ) -> Json<Value> {
        state.lock().expect("quota state").b_requests += 1;
        Json(json!({
            "id": "chatcmpl-fallback",
            "object": "chat.completion",
            "model": "fallback-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "fallback succeeded"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }))
    }

    async fn start_quota_upstream() -> (
        String,
        Arc<Mutex<QuotaMockState>>,
        oneshot::Sender<()>,
        JoinHandle<()>,
    ) {
        let state = Arc::new(Mutex::new(QuotaMockState::default()));
        let app = Router::new()
            .route("/a/v1/chat/completions", post(quota_a))
            .route("/b/v1/chat/completions", post(quota_b))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind quota upstream");
        let address = listener.local_addr().expect("quota address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("quota upstream server");
        });
        (format!("http://{address}"), state, shutdown_tx, task)
    }

    #[derive(Default)]
    struct RealmMockState {
        chat_requests: Vec<Value>,
        native_requests: Vec<Value>,
        failed_native_requests: usize,
    }

    async fn realm_chat(
        State(state): State<Arc<Mutex<RealmMockState>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.lock().expect("realm state").chat_requests.push(body);
        Json(json!({
            "id": "chatcmpl-realm",
            "object": "chat.completion",
            "model": "realm-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "REALM_HANDOFF_SUMMARY"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        }))
    }

    async fn realm_native_compact(
        State(state): State<Arc<Mutex<RealmMockState>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let mut state = state.lock().expect("realm state");
        state.native_requests.push(body);
        let id = format!("cmp_native_e2e_{}", state.native_requests.len());
        Json(json!({
            "output": [{
                "id": id,
                "type": "compaction",
                "encrypted_content": "opaque-native-e2e"
            }]
        }))
    }

    async fn realm_native_response(
        State(state): State<Arc<Mutex<RealmMockState>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state
            .lock()
            .expect("realm state")
            .native_requests
            .push(body);
        Json(json!({
            "id": "resp-native-e2e",
            "object": "response",
            "status": "completed",
            "model": "native-model",
            "output": [{
                "id": "msg-native-e2e",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type":"output_text","text":"native continued","annotations":[]}]
            }],
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
        }))
    }

    async fn realm_native_compact_unavailable(
        State(state): State<Arc<Mutex<RealmMockState>>>,
        Json(body): Json<Value>,
    ) -> (axum::http::StatusCode, Json<Value>) {
        let mut state = state.lock().expect("realm state");
        state.failed_native_requests += 1;
        state.native_requests.push(body);
        (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": {
                    "type": "insufficient_quota",
                    "code": "usage_limit_reached",
                    "message": "official compaction quota exhausted"
                }
            })),
        )
    }

    async fn streaming_native_compact(Json(_body): Json<Value>) -> axum::response::Response {
        let frames = vec![
            (
                std::time::Duration::ZERO,
                "event: response.created\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-stream\",\"status\":\"in_progress\",\"output\":[]}}\r\n\r\n",
            ),
            (
                std::time::Duration::from_millis(300),
                "event: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"cmp_stream_e2e\",\"type\":\"compaction\",\"encrypted_content\":\"opaque-stream-e2e\"}}\r\n\r\n",
            ),
            (
                std::time::Duration::ZERO,
                "event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-stream\",\"status\":\"completed\",\"output\":[{\"id\":\"cmp_stream_e2e\",\"type\":\"compaction\",\"encrypted_content\":\"opaque-stream-e2e\"}]}}\r\n\r\n",
            ),
        ];
        let stream = futures::stream::iter(frames).then(|(delay, frame)| async move {
            tokio::time::sleep(delay).await;
            Ok::<_, std::io::Error>(Bytes::from_static(frame.as_bytes()))
        });
        let mut response = axum::response::Response::new(axum::body::Body::from_stream(stream));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        response
    }

    async fn start_realm_upstream() -> (
        String,
        Arc<Mutex<RealmMockState>>,
        oneshot::Sender<()>,
        JoinHandle<()>,
    ) {
        let state = Arc::new(Mutex::new(RealmMockState::default()));
        let app = Router::new()
            .route("/chat/v1/chat/completions", post(realm_chat))
            .route("/native/v1/responses/compact", post(realm_native_compact))
            .route("/native/v1/responses", post(realm_native_response))
            .route(
                "/failing-native/v1/responses/compact",
                post(realm_native_compact_unavailable),
            )
            .route(
                "/stream/v1/responses/compact",
                post(streaming_native_compact),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind realm upstream");
        let address = listener.local_addr().expect("realm address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("realm upstream server");
        });
        (format!("http://{address}"), state, shutdown_tx, task)
    }

    fn select_codex_provider(db: &Database, id: &str) {
        db.set_current_provider("codex", id)
            .expect("set selected provider in db");
        crate::settings::set_current_provider(&crate::app_config::AppType::Codex, Some(id))
            .expect("set selected effective provider");
    }

    fn seed_chat_provider(db: &Database, base_url: &str) {
        let mut provider = crate::provider::Provider::with_id(
            "continuity-chat".to_string(),
            "Continuity Chat".to_string(),
            json!({
                "base_url": base_url,
                "api_format": "openai_chat",
                "auth": {"OPENAI_API_KEY": "e2e-provider-key"}
            }),
            None,
        );
        provider.sort_index = Some(1);
        db.save_provider("codex", &provider)
            .expect("save e2e provider");
        db.set_current_provider("codex", &provider.id)
            .expect("set db provider");
        crate::settings::set_current_provider(
            &crate::app_config::AppType::Codex,
            Some(&provider.id),
        )
        .expect("set effective provider");
    }

    fn test_proxy_config() -> ProxyConfig {
        ProxyConfig {
            listen_address: "127.0.0.1".to_string(),
            listen_port: 0,
            ..ProxyConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn local_compact_survives_proxy_and_database_restart_end_to_end() {
        let environment = TestEnvironment::new();
        let (upstream_url, captured, shutdown_tx, upstream_task) = start_mock_chat_upstream().await;
        let db = Arc::new(Database::init().expect("disk database"));
        seed_chat_provider(&db, &upstream_url);
        let server = ProxyServer::new(test_proxy_config(), db, None);
        let info = server.start().await.expect("start proxy");
        let client = reqwest::Client::new();
        let secret = "EARLIEST_E2E_CONTEXT_SHOULD_BE_ENCRYPTED";

        let compact_response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses/compact",
                info.port
            ))
            .header("thread-id", "thread-e2e")
            .header("session-id", "session-e2e")
            .header("x-client-request-id", "compact-request-e2e")
            .json(&json!({
                "model": "mock-chat",
                "stream": false,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type":"input_text","text":secret}]
                }]
            }))
            .send()
            .await
            .expect("compact request");
        assert_eq!(compact_response.status(), reqwest::StatusCode::OK);
        let compact_json: Value = compact_response.json().await.expect("compact json");
        let item = compact_json["output"][0].clone();
        assert_eq!(item["type"], "compaction");
        assert!(item["encrypted_content"]
            .as_str()
            .is_some_and(|value| value.starts_with("bcmp1.")));

        server.stop().await.expect("stop first proxy");
        drop(server);

        // Reopen the on-disk DB and reconstruct every proxy service. The envelope
        // must still resolve without any in-memory history from the first process.
        let reopened = Arc::new(Database::init().expect("reopen database"));
        let restarted = ProxyServer::new(test_proxy_config(), reopened, None);
        let restarted_info = restarted.start().await.expect("restart proxy");
        let resume_response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses",
                restarted_info.port
            ))
            .header("thread-id", "thread-e2e")
            .header("session-id", "session-e2e")
            .header("x-client-request-id", "resume-request-e2e")
            .json(&json!({
                "model": "mock-chat",
                "stream": false,
                "input": [
                    item,
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"E2E_SUFFIX_ONCE"}]}
                ]
            }))
            .send()
            .await
            .expect("resume request");
        assert_eq!(resume_response.status(), reqwest::StatusCode::OK);
        let resumed: Value = resume_response.json().await.expect("resume json");
        assert_eq!(resumed["status"], "completed");

        restarted.stop().await.expect("stop restarted proxy");
        let requests = captured.lock().expect("captured requests");
        assert!(requests.len() >= 2, "compact + resume must reach upstream");
        let compact_wire = requests[0].to_string();
        assert!(compact_wire.contains(secret));
        assert!(compact_wire.contains("CONTEXT CHECKPOINT COMPACTION"));
        let resume_wire = requests.last().unwrap().to_string();
        assert!(resume_wire.contains("HANDOFF_E2E_SUMMARY"));
        assert_eq!(resume_wire.matches("E2E_SUFFIX_ONCE").count(), 1);
        assert!(!resume_wire.contains("\"type\":\"compaction\""));
        drop(requests);

        let database_bytes = std::fs::read(environment.db_path()).expect("read sqlite database");
        assert!(!database_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));

        let _ = shutdown_tx.send(());
        upstream_task.await.expect("join mock upstream");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn quota_429_falls_back_then_future_request_skips_latched_provider_end_to_end() {
        let _environment = TestEnvironment::new();
        let (upstream_origin, counts, shutdown_tx, upstream_task) = start_quota_upstream().await;
        let db = Arc::new(Database::init().expect("quota e2e database"));
        for (id, suffix, order) in [("quota-a", "a", 1), ("quota-b", "b", 2)] {
            let mut provider = crate::provider::Provider::with_id(
                id.to_string(),
                id.to_string(),
                json!({
                    "base_url": format!("{upstream_origin}/{suffix}/v1"),
                    "api_format": "openai_chat",
                    "auth": {"OPENAI_API_KEY": "quota-e2e-key"}
                }),
                None,
            );
            provider.sort_index = Some(order);
            db.save_provider("codex", &provider)
                .expect("save quota provider");
            db.add_to_failover_queue("codex", id)
                .expect("queue quota provider");
        }
        db.set_current_provider("codex", "quota-a")
            .expect("set quota current");
        crate::settings::set_current_provider(&crate::app_config::AppType::Codex, Some("quota-a"))
            .expect("set effective quota provider");
        let mut app_config = db.get_proxy_config_for_app("codex").await.unwrap();
        app_config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(app_config).await.unwrap();

        let server = ProxyServer::new(test_proxy_config(), db.clone(), None);
        let info = server.start().await.expect("start quota proxy");
        let client = reqwest::Client::new();
        for request_id in ["quota-request-1", "quota-request-2"] {
            let response = client
                .post(format!("http://127.0.0.1:{}/v1/responses", info.port))
                .header("session-id", "quota-session")
                .header("x-client-request-id", request_id)
                .json(&json!({
                    "model": "mock-chat",
                    "stream": false,
                    "input": [{"type":"message","role":"user","content":"review this"}]
                }))
                .send()
                .await
                .expect("quota proxy request");
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let body: Value = response.json().await.expect("quota response json");
            assert_eq!(body["status"], "completed");
        }
        server.stop().await.expect("stop quota proxy");

        let state = counts.lock().expect("quota counts");
        assert_eq!(
            state.a_requests, 1,
            "latched A must be skipped on request 2"
        );
        assert_eq!(state.b_requests, 2, "B serves fallback and next request");
        drop(state);
        let health = db.get_provider_health("quota-a", "codex").await.unwrap();
        assert!(
            health.is_healthy,
            "quota exhaustion must not poison circuit health"
        );

        let _ = shutdown_tx.send(());
        upstream_task.await.expect("join quota upstream");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn native_and_chat_compactions_migrate_both_directions_end_to_end() {
        let _environment = TestEnvironment::new();
        let (origin, captured, shutdown_tx, upstream_task) = start_realm_upstream().await;
        let db = Arc::new(Database::init().expect("realm e2e database"));
        for (id, suffix, api_format) in [
            ("realm-native", "native", "openai_responses"),
            ("realm-chat", "chat", "openai_chat"),
        ] {
            let provider = crate::provider::Provider::with_id(
                id.to_string(),
                id.to_string(),
                json!({
                    "base_url": format!("{origin}/{suffix}/v1"),
                    "api_format": api_format,
                    "auth": {"OPENAI_API_KEY": "realm-e2e-key"}
                }),
                None,
            );
            db.save_provider("codex", &provider).unwrap();
        }
        select_codex_provider(&db, "realm-native");
        let server = ProxyServer::new(test_proxy_config(), db.clone(), None);
        let info = server.start().await.expect("start realm proxy");
        let client = reqwest::Client::new();

        // Native opaque token -> Chat: the proxy indexes the native token against
        // its encrypted canonical snapshot, then expands that snapshot before the
        // Chat transform. No opaque token reaches the incompatible gateway.
        let native_compact = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses/compact",
                info.port
            ))
            .header("thread-id", "realm-thread-native")
            .header("session-id", "realm-session-native")
            .json(&json!({
                "model":"native-model",
                "input":[{"type":"message","role":"user","content":"NATIVE_EARLY_CONTEXT"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(native_compact.status(), reqwest::StatusCode::OK);
        let native_item = native_compact.json::<Value>().await.unwrap()["output"][0].clone();
        select_codex_provider(&db, "realm-chat");
        let chat_resume = client
            .post(format!("http://127.0.0.1:{}/v1/responses", info.port))
            .header("thread-id", "realm-thread-native")
            .header("session-id", "realm-session-native")
            .json(&json!({
                "model":"chat-model",
                "input":[native_item,{"type":"message","role":"user","content":"NATIVE_TO_CHAT_SUFFIX"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(chat_resume.status(), reqwest::StatusCode::OK);

        // Bridge token -> native Responses: recompact the bridge snapshot through
        // the official compact endpoint, then resume with the returned native
        // token and preserve the continuation suffix exactly once.
        let bridge_compact = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses/compact",
                info.port
            ))
            .header("thread-id", "realm-thread-bridge")
            .header("session-id", "realm-session-bridge")
            .json(&json!({
                "model":"chat-model",
                "input":[{"type":"message","role":"user","content":"BRIDGE_EARLY_CONTEXT"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(bridge_compact.status(), reqwest::StatusCode::OK);
        let bridge_item = bridge_compact.json::<Value>().await.unwrap()["output"][0].clone();
        select_codex_provider(&db, "realm-native");
        let native_resume = client
            .post(format!("http://127.0.0.1:{}/v1/responses", info.port))
            .header("thread-id", "realm-thread-bridge")
            .header("session-id", "realm-session-bridge")
            .json(&json!({
                "model":"native-model",
                "input":[bridge_item,{"type":"message","role":"user","content":"BRIDGE_TO_NATIVE_SUFFIX"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(native_resume.status(), reqwest::StatusCode::OK);
        server.stop().await.unwrap();

        let state = captured.lock().expect("realm captured");
        let chat_wire = state
            .chat_requests
            .iter()
            .map(Value::to_string)
            .find(|body| body.contains("NATIVE_TO_CHAT_SUFFIX"))
            .expect("native->chat request");
        assert!(chat_wire.contains("NATIVE_EARLY_CONTEXT"));
        assert_eq!(chat_wire.matches("NATIVE_TO_CHAT_SUFFIX").count(), 1);
        assert!(!chat_wire.contains("opaque-native-e2e"));

        let native_wire = state
            .native_requests
            .iter()
            .map(Value::to_string)
            .find(|body| body.contains("BRIDGE_TO_NATIVE_SUFFIX"))
            .expect("bridge->native request");
        let official_recompact_wire = state
            .native_requests
            .iter()
            .map(Value::to_string)
            .find(|body| body.contains("BRIDGE_EARLY_CONTEXT"))
            .expect("bridge snapshot sent to official compact endpoint");
        assert!(!official_recompact_wire.contains("bcmp1."));
        assert!(native_wire.contains("opaque-native-e2e"));
        assert_eq!(native_wire.matches("BRIDGE_TO_NATIVE_SUFFIX").count(), 1);
        assert!(!native_wire.contains("bcmp1."));
        drop(state);

        let _ = shutdown_tx.send(());
        upstream_task.await.expect("join realm upstream");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn official_compact_failure_uses_hierarchical_bridge_executor_end_to_end() {
        let _environment = TestEnvironment::new();
        let (origin, captured, shutdown_tx, upstream_task) = start_realm_upstream().await;
        let db = Arc::new(Database::init().expect("hierarchical failover database"));
        for (id, suffix, api_format, order) in [
            (
                "hierarchical-native",
                "failing-native",
                "openai_responses",
                1,
            ),
            ("hierarchical-chat", "chat", "openai_chat", 2),
        ] {
            let mut provider = crate::provider::Provider::with_id(
                id.to_string(),
                id.to_string(),
                json!({
                    "base_url": format!("{origin}/{suffix}/v1"),
                    "api_format": api_format,
                    "auth": {"OPENAI_API_KEY": "hierarchical-e2e-key"}
                }),
                None,
            );
            provider.sort_index = Some(order);
            db.save_provider("codex", &provider).unwrap();
            db.add_to_failover_queue("codex", id).unwrap();
        }
        select_codex_provider(&db, "hierarchical-native");
        let mut app_config = db.get_proxy_config_for_app("codex").await.unwrap();
        app_config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(app_config).await.unwrap();

        let server = ProxyServer::new(test_proxy_config(), db, None);
        let info = server.start().await.expect("start hierarchical proxy");
        let client = reqwest::Client::new();
        let input = (0..6)
            .map(|index| {
                json!({
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!("HIERARCHICAL_SEGMENT_{index}_{}", "x".repeat(220_000))
                    }]
                })
            })
            .collect::<Vec<_>>();
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses/compact",
                info.port
            ))
            .header("thread-id", "hierarchical-failover-thread")
            .header("session-id", "hierarchical-failover-session")
            .json(&json!({
                "model": "large-context-model",
                "stream": false,
                "input": input
            }))
            .send()
            .await
            .expect("hierarchical compact request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let response_body: Value = response.json().await.expect("hierarchical compact json");
        assert!(response_body["output"][0]["encrypted_content"]
            .as_str()
            .is_some_and(|value| value.starts_with("bcmp1.")));
        server.stop().await.expect("stop hierarchical proxy");

        let state = captured.lock().expect("hierarchical captured state");
        assert_eq!(state.failed_native_requests, 1);
        assert!(
            state.chat_requests.len() >= 2,
            "large context must make chunk calls followed by a final handoff call"
        );
        let final_request = state.chat_requests.last().unwrap().to_string();
        assert!(final_request.contains("Earlier chronological checkpoint"));
        assert!(!final_request.contains("Older chronological segment"));
        drop(state);

        let _ = shutdown_tx.send(());
        upstream_task.await.expect("join hierarchical upstream");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn native_compaction_sse_streams_before_completion_and_is_immediately_resumable() {
        let _environment = TestEnvironment::new();
        let (origin, captured, shutdown_tx, upstream_task) = start_realm_upstream().await;
        let db = Arc::new(Database::init().expect("stream e2e database"));
        let native = crate::provider::Provider::with_id(
            "stream-native".to_string(),
            "Stream Native".to_string(),
            json!({
                "base_url": format!("{origin}/stream/v1"),
                "api_format": "openai_responses",
                "auth": {"OPENAI_API_KEY": "stream-e2e-key"}
            }),
            None,
        );
        db.save_provider("codex", &native).unwrap();
        let chat = crate::provider::Provider::with_id(
            "stream-chat".to_string(),
            "Stream Chat".to_string(),
            json!({
                "base_url": format!("{origin}/chat/v1"),
                "api_format": "openai_chat",
                "auth": {"OPENAI_API_KEY": "stream-e2e-key"}
            }),
            None,
        );
        db.save_provider("codex", &chat).unwrap();
        select_codex_provider(&db, "stream-native");
        let server = ProxyServer::new(test_proxy_config(), db.clone(), None);
        let info = server.start().await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/responses/compact",
                info.port
            ))
            .header("thread-id", "stream-thread")
            .header("session-id", "stream-session")
            .json(&json!({
                "model":"native-model",
                "stream":true,
                "input":[{"type":"message","role":"user","content":"STREAM_EARLY_CONTEXT"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let mut bytes = response.bytes_stream();
        let first = tokio::time::timeout(std::time::Duration::from_millis(150), bytes.next())
            .await
            .expect("first native SSE event must not wait for compact completion")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("response.created"));
        let mut wire = first.to_vec();
        while let Some(chunk) = bytes.next().await {
            wire.extend_from_slice(&chunk.unwrap());
        }
        let wire = String::from_utf8(wire).unwrap();
        assert!(wire.contains("cmp_stream_e2e"));
        assert!(
            wire.contains("\r\n\r\n"),
            "SSE framing must remain byte-compatible"
        );

        select_codex_provider(&db, "stream-chat");
        let resume = client
            .post(format!("http://127.0.0.1:{}/v1/responses", info.port))
            .header("thread-id", "stream-thread")
            .header("session-id", "stream-session")
            .json(&json!({
                "model":"chat-model",
                "stream":false,
                "input":[
                    {"id":"cmp_stream_e2e","type":"compaction","encrypted_content":"opaque-stream-e2e"},
                    {"type":"message","role":"user","content":"STREAM_SUFFIX"}
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resume.status(), reqwest::StatusCode::OK);
        server.stop().await.unwrap();
        let state = captured.lock().unwrap();
        let chat_wire = state.chat_requests.last().unwrap().to_string();
        assert!(chat_wire.contains("STREAM_EARLY_CONTEXT"));
        assert_eq!(chat_wire.matches("STREAM_SUFFIX").count(), 1);
        assert!(!chat_wire.contains("opaque-stream-e2e"));
        drop(state);
        let _ = shutdown_tx.send(());
        upstream_task.await.unwrap();
    }
}
