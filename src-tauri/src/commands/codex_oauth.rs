//! Codex OAuth Tauri Commands
//!
//! 提供 OpenAI ChatGPT Plus/Pro OAuth 认证相关的 Tauri 命令。
//!
//! 大部分认证命令通过通用 `auth_*` 命令（参见 `commands::auth`）暴露给前端，
//! 此处定义 State wrapper 以及 Codex OAuth 专属的订阅额度和模型列表查询命令。

use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::services::model_fetch::FetchedModel;
use crate::services::subscription::{query_codex_quota, CredentialStatus, SubscriptionQuota};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::State;
use tokio::sync::{Mutex, RwLock};

/// Codex OAuth 认证状态
pub struct CodexOAuthState(pub Arc<RwLock<CodexOAuthManager>>, pub Arc<Mutex<()>>);

const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetCredit {
    pub id: String,
    #[serde(default, alias = "reset_type")]
    pub reset_type: Option<String>,
    pub status: String,
    #[serde(default, alias = "expires_at")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetCredits {
    #[serde(default, alias = "available_count")]
    pub available_count: i64,
    #[serde(default)]
    pub credits: Vec<CodexResetCredit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetResult {
    pub code: String,
    pub windows_reset: Option<i64>,
}

async fn token_and_account(
    account_id: Option<String>,
    state: &State<'_, CodexOAuthState>,
) -> Result<(String, String), String> {
    let manager = state.0.read().await;
    let id = match account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_string(),
        None => manager
            .default_account_id()
            .await
            .ok_or_else(|| "No ChatGPT account available".to_string())?,
    };
    let token = manager
        .get_valid_token_for_account(&id)
        .await
        .map_err(|error| format!("Codex OAuth token unavailable: {error}"))?;
    Ok((token, id))
}

async fn fetch_reset_credits(token: &str, account_id: &str) -> Result<CodexResetCredits, String> {
    let response = crate::proxy::http_client::get()
        .get(RESET_CREDITS_URL)
        .bearer_auth(token)
        .header("ChatGPT-Account-Id", account_id)
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| format!("Unable to query reset credits: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Unable to query reset credits (HTTP {status}): {body}"
        ));
    }
    response
        .json::<CodexResetCredits>()
        .await
        .map_err(|error| format!("Invalid reset credit response: {error}"))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_codex_oauth_reset_credits(
    account_id: Option<String>,
    state: State<'_, CodexOAuthState>,
) -> Result<CodexResetCredits, String> {
    let (token, account_id) = token_and_account(account_id, &state).await?;
    fetch_reset_credits(&token, &account_id).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn consume_codex_oauth_reset(
    account_id: String,
    credit_id: String,
    state: State<'_, CodexOAuthState>,
) -> Result<CodexResetResult, String> {
    // Reset credits are scarce and the upstream consume endpoint is not
    // idempotent. Serialize verification + consumption inside this app so a
    // double click or two windows cannot spend the same credit concurrently.
    let _reset_guard = state.1.try_lock().map_err(|_| {
        "A reset is already in progress. Wait for it to finish before trying again.".to_string()
    })?;
    let account_id = account_id.trim().to_string();
    let credit_id = credit_id.trim().to_string();
    if account_id.is_empty() || credit_id.is_empty() {
        return Err("accountId and creditId are required".to_string());
    }
    let (token, resolved_account_id) = token_and_account(Some(account_id.clone()), &state).await?;
    let available = fetch_reset_credits(&token, &resolved_account_id).await?;
    if !available
        .credits
        .iter()
        .any(|credit| credit.id == credit_id && credit.status == "available")
    {
        return Err("The selected reset is no longer available".to_string());
    }

    let response = crate::proxy::http_client::get()
        .post(format!("{RESET_CREDITS_URL}/consume"))
        .bearer_auth(&token)
        .header("ChatGPT-Account-Id", &resolved_account_id)
        .header("User-Agent", "codex-cli")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "redeem_request_id": uuid::Uuid::new_v4().to_string(),
            "credit_id": credit_id,
        }))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| format!("Reset request failed: {error}"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| format!("Invalid reset response: {error}"))?;
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Reset failed");
        return Err(format!("{message} (HTTP {status})"));
    }
    Ok(CodexResetResult {
        code: body
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        windows_reset: body.get("windows_reset").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod reset_credit_tests {
    use super::*;

    #[test]
    fn reset_credit_payload_accepts_upstream_snake_case_and_emits_camel_case() {
        let payload = r#"{
            "available_count": 1,
            "credits": [{
                "id": "credit-1",
                "reset_type": "full",
                "status": "available",
                "expires_at": "2026-07-20T00:00:00Z"
            }]
        }"#;
        let parsed: CodexResetCredits = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.available_count, 1);
        assert_eq!(parsed.credits[0].reset_type.as_deref(), Some("full"));

        let frontend = serde_json::to_value(parsed).unwrap();
        assert_eq!(frontend["availableCount"], 1);
        assert_eq!(frontend["credits"][0]["resetType"], "full");
        assert_eq!(frontend["credits"][0]["expiresAt"], "2026-07-20T00:00:00Z");
    }
}

/// 查询 Codex OAuth (ChatGPT Plus/Pro) 订阅额度
///
/// - `account_id` 未指定时回退到 `CodexOAuthManager` 的默认账号
/// - 没有任何账号时返回 `not_found`，前端 `SubscriptionQuotaView` 会静默不渲染
/// - 复用 `services::subscription::query_codex_quota`，因此 wham/usage 端点协议
///   与 Codex CLI 路径完全一致
#[tauri::command(rename_all = "camelCase")]
pub async fn get_codex_oauth_quota(
    account_id: Option<String>,
    force_refresh: Option<bool>,
    state: State<'_, CodexOAuthState>,
) -> Result<SubscriptionQuota, String> {
    let manager = state.0.read().await;

    // 解析最终使用的账号 ID：显式 > 默认账号 > 无账号 (not_found)
    let resolved = match account_id {
        Some(id) => Some(id),
        None => manager.default_account_id().await,
    };
    let Some(id) = resolved else {
        return Ok(SubscriptionQuota::not_found("codex_oauth"));
    };

    // 获取（必要时自动刷新）access_token
    let token = match manager.get_valid_token_for_account(&id).await {
        Ok(t) => t,
        Err(e) => {
            return Ok(SubscriptionQuota::error(
                "codex_oauth",
                CredentialStatus::Expired,
                format!("Codex OAuth token unavailable: {e}"),
            ));
        }
    };

    // 瞬时传输失败以 Err 传播（前端 reject → retry + 保留上次成功值）。
    let quota = query_codex_quota(
        &token,
        Some(&id),
        "codex_oauth",
        "Codex OAuth access token expired or rejected. Please re-login via cc-switch.",
        force_refresh.unwrap_or(false),
    )
    .await?;

    if !matches!(quota.credential_status, CredentialStatus::Expired) {
        return Ok(quota);
    }

    // JWT expiry is only a local hint. OpenAI can invalidate an otherwise
    // unexpired access token server-side. Force one refresh after a 401/403 and
    // retry the quota request once before asking the user to sign in again.
    let refreshed_token = match manager.refresh_token_after_rejection(&id, &token).await {
        Ok(token) => token,
        Err(error) => {
            return Ok(SubscriptionQuota::error(
                "codex_oauth",
                CredentialStatus::Expired,
                format!(
                    "Codex OAuth refresh failed after the access token was rejected: {error}. Please re-login via cc-switch."
                ),
            ));
        }
    };

    query_codex_quota(
        &refreshed_token,
        Some(&id),
        "codex_oauth",
        "Codex OAuth access token remained rejected after one automatic refresh. Please re-login via cc-switch.",
        true,
    )
    .await
}

/// 获取 Codex OAuth (ChatGPT Plus/Pro) 可用模型列表
///
/// ChatGPT Codex 反代使用 `chatgpt.com/backend-api/codex/*`，不是 OpenAI 兼容
/// `/v1/models`。这里复用托管 OAuth 账号的 access_token，直接读取 Codex 后端
/// 暴露的模型列表端点。
#[tauri::command(rename_all = "camelCase")]
pub async fn get_codex_oauth_models(
    account_id: Option<String>,
    state: State<'_, CodexOAuthState>,
) -> Result<Vec<FetchedModel>, String> {
    let manager = state.0.read().await;
    let resolved = match account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => manager.default_account_id().await,
    };
    let Some(id) = resolved else {
        return Err("No ChatGPT account available".to_string());
    };

    let token = manager
        .get_valid_token_for_account(&id)
        .await
        .map_err(|e| format!("Codex OAuth token unavailable: {e}"))?;

    crate::services::codex_oauth_models::fetch_models_with_token(&token, &id).await
}
