use crate::{database::Database, error::AppError, provider::Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SETTINGS_KEY: &str = "codex_model_routes";
const PROVIDER_KEY: &str = "codex_model_route_provider_id";

fn is_managed_virtual_model(model: &str, display_name: Option<&str>) -> bool {
    let model = model.trim();
    model.starts_with("ccs-")
        || model.eq_ignore_ascii_case(super::auto_review::AUTO_REVIEW_MODEL)
        || display_name.is_some_and(|name| name.trim().eq_ignore_ascii_case("Codex Auto Review"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogReasoningLevel {
    pub effort: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelInput {
    pub model: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CatalogReasoningLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoute {
    pub alias: String,
    pub provider_id: String,
    pub upstream_model: String,
    pub display_name: String,
    #[serde(default)]
    pub context_window: Option<u64>,
}

fn standard_reasoning_levels() -> Vec<CatalogReasoningLevel> {
    [
        ("low", "Fast responses with lighter reasoning"),
        (
            "medium",
            "Balances speed and reasoning depth for everyday tasks",
        ),
        ("high", "Greater reasoning depth for complex problems"),
        ("xhigh", "Extra high reasoning depth for complex problems"),
    ]
    .into_iter()
    .map(|(effort, description)| CatalogReasoningLevel {
        effort: effort.to_string(),
        description: description.to_string(),
    })
    .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelRouteSettings {
    #[serde(default)]
    pub official_models: Vec<CatalogModelInput>,
    #[serde(default)]
    pub routes: Vec<ModelRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedModelRoute {
    Official,
    ThirdParty(ModelRoute),
}

pub fn load(db: &Database) -> Result<ModelRouteSettings, AppError> {
    let Some(raw) = db.get_setting(SETTINGS_KEY)? else {
        return Ok(ModelRouteSettings::default());
    };
    serde_json::from_str(&raw)
        .map_err(|error| AppError::Config(format!("Invalid Codex model routes: {error}")))
}

pub fn save(db: &Database, settings: &ModelRouteSettings) -> Result<(), AppError> {
    let raw = serde_json::to_string(settings)
        .map_err(|error| AppError::Config(format!("Unable to save Codex model routes: {error}")))?;
    db.set_setting(SETTINGS_KEY, &raw)
}

pub fn clear(db: &Database) -> Result<(), AppError> {
    save(db, &ModelRouteSettings::default())
}

pub fn remember_provider(db: &Database, provider_id: &str) -> Result<(), AppError> {
    db.set_setting(PROVIDER_KEY, provider_id.trim())
}

pub fn forget_provider(db: &Database) -> Result<(), AppError> {
    db.set_setting(PROVIDER_KEY, "")
}

fn provider_catalog_models(provider: &Provider) -> Vec<CatalogModelInput> {
    provider
        .settings_config
        .pointer("/modelCatalog/models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| serde_json::from_value(model.clone()).ok())
        .filter(|model: &CatalogModelInput| !model.model.trim().is_empty())
        .collect()
}

fn ensure_configured_with_official_models(
    db: &Database,
    official_models: Vec<CatalogModelInput>,
) -> Result<ModelRouteSettings, AppError> {
    let existing = load(db)?;
    if !existing.routes.is_empty() {
        return Ok(existing);
    }

    let providers = db.get_all_providers("codex")?;
    let remembered = db
        .get_setting(PROVIDER_KEY)?
        .filter(|provider_id| !provider_id.trim().is_empty());
    let candidates = providers
        .values()
        .filter(|provider| {
            provider.category.as_deref() != Some("official")
                && !provider_catalog_models(provider).is_empty()
        })
        .collect::<Vec<_>>();
    let provider = remembered
        .as_deref()
        .and_then(|provider_id| {
            candidates
                .iter()
                .copied()
                .find(|provider| provider.id == provider_id)
        })
        .or_else(|| (candidates.len() == 1).then(|| candidates[0]));
    let Some(provider) = provider else {
        return Ok(existing);
    };
    if official_models.is_empty() {
        return Err(AppError::Config(
            "Codex Desktop official model cache is empty; load the native model menu first"
                .to_string(),
        ));
    }

    let settings = build_settings(
        &provider.id,
        &provider.name,
        official_models,
        provider_catalog_models(provider),
    )?;
    save(db, &settings)?;
    remember_provider(db, &provider.id)?;
    Ok(settings)
}

/// Repair the durable route projection before takeover. Older builds could
/// leave the provider's selected modelCatalog intact while clearing the route
/// table, so merely opening the proxy produced an official-only/stale menu.
pub fn ensure_configured_for_takeover(db: &Database) -> Result<ModelRouteSettings, AppError> {
    ensure_configured_with_official_models(db, cached_official_models()?)
}

/// Persist the model choices in the provider SSOT without applying the provider
/// to Codex live files. This is deliberately separate from ProviderService::update:
/// updating the currently selected third-party provider may also replace
/// `~/.codex/auth.json`, which would destroy the user's ChatGPT authentication.
pub fn with_provider_model_catalog(
    provider: &Provider,
    models: &[CatalogModelInput],
) -> Result<Provider, AppError> {
    if models.is_empty() {
        return Err(AppError::InvalidInput(
            "Select at least one third-party model".to_string(),
        ));
    }

    let mut updated = provider.clone();
    if !updated.settings_config.is_object() {
        updated.settings_config = serde_json::json!({});
    }
    updated.settings_config["modelCatalog"] = serde_json::json!({
        "models": models,
    });
    Ok(updated)
}

pub fn without_provider_model_catalog(provider: &Provider) -> Provider {
    let mut updated = provider.clone();
    if let Some(settings) = updated.settings_config.as_object_mut() {
        settings.remove("modelCatalog");
    }
    updated
}

pub fn cached_official_models() -> Result<Vec<CatalogModelInput>, AppError> {
    let path = crate::codex_config::get_codex_config_dir().join("models_cache.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))?;
    let value: Value = serde_json::from_str(&text).map_err(|error| AppError::json(&path, error))?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let model = entry.get("slug").and_then(Value::as_str)?.trim();
            let display_name = entry.get("display_name").and_then(Value::as_str);
            if model.is_empty() || is_managed_virtual_model(model, display_name) {
                return None;
            }
            Some(CatalogModelInput {
                model: model.to_string(),
                display_name: display_name.map(str::to_string),
                context_window: entry.get("context_window").and_then(Value::as_u64),
                default_reasoning_level: entry
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                supported_reasoning_levels: entry
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|level| {
                        Some(CatalogReasoningLevel {
                            effort: level.get("effort")?.as_str()?.to_string(),
                            description: level
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect(),
            })
        })
        .collect();
    Ok(models)
}

pub fn build_settings(
    provider_id: &str,
    provider_name: &str,
    official_models: Vec<CatalogModelInput>,
    third_party_models: Vec<CatalogModelInput>,
) -> Result<ModelRouteSettings, AppError> {
    let official_models = official_models
        .into_iter()
        .filter(|model| !is_managed_virtual_model(&model.model, model.display_name.as_deref()))
        .collect::<Vec<_>>();
    if official_models.is_empty() {
        return Err(AppError::InvalidInput(
            "Official Codex model list is required for coexistence mode".to_string(),
        ));
    }
    if third_party_models.is_empty() {
        return Err(AppError::InvalidInput(
            "Select at least one third-party model".to_string(),
        ));
    }
    let short_provider = provider_id.chars().take(8).collect::<String>();
    let mut routes = Vec::new();
    for (index, model) in third_party_models.into_iter().enumerate() {
        let upstream = model.model.trim();
        if upstream.is_empty() {
            continue;
        }
        let safe_model = upstream
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        routes.push(ModelRoute {
            alias: format!("ccs-{short_provider}-{index}-{safe_model}"),
            provider_id: provider_id.to_string(),
            upstream_model: upstream.to_string(),
            display_name: format!(
                "[{provider_name}] {}",
                model.display_name.unwrap_or_else(|| upstream.to_string())
            ),
            context_window: model.context_window,
        });
    }
    if routes.is_empty() {
        return Err(AppError::InvalidInput(
            "Select at least one valid third-party model".to_string(),
        ));
    }
    Ok(ModelRouteSettings {
        official_models,
        routes,
    })
}

pub fn resolve(
    db: &Database,
    requested_model: &str,
) -> Result<Option<ResolvedModelRoute>, AppError> {
    let settings = load(db)?;
    if settings.routes.is_empty() {
        return Ok(None);
    }
    if let Some(route) = settings
        .routes
        .into_iter()
        .find(|route| route.alias == requested_model)
    {
        return Ok(Some(ResolvedModelRoute::ThirdParty(route)));
    }
    Ok(Some(ResolvedModelRoute::Official))
}

pub fn augment_settings(db: &Database, base: &Value) -> Result<Value, AppError> {
    let settings = load(db)?;
    if settings.routes.is_empty() {
        return Ok(base.clone());
    }
    let mut combined = Vec::new();
    for model in settings.official_models {
        if is_managed_virtual_model(&model.model, model.display_name.as_deref()) {
            continue;
        }
        let levels = if model.supported_reasoning_levels.is_empty() {
            standard_reasoning_levels()
        } else {
            model.supported_reasoning_levels
        };
        combined.push(serde_json::json!({
            "model": model.model,
            "displayName": model.display_name,
            "contextWindow": model.context_window,
            "defaultReasoningLevel": model.default_reasoning_level.unwrap_or_else(|| "medium".to_string()),
            "supportedReasoningLevels": levels,
        }));
    }
    for route in settings.routes {
        combined.push(serde_json::json!({
            "model": route.alias,
            "displayName": route.display_name,
            "contextWindow": route.context_window,
            "defaultReasoningLevel": "high",
            "supportedReasoningLevels": standard_reasoning_levels(),
        }));
    }
    let mut output = base.clone();
    output["modelCatalog"] = serde_json::json!({ "models": combined });
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_catalog_update_preserves_provider_auth_and_other_settings() {
        let provider = Provider::with_id(
            "third-party".into(),
            "Third Party".into(),
            serde_json::json!({
                "auth": {
                    "OPENAI_API_KEY": "third-party-key"
                },
                "config": "model = 'old-model'\nbase_url = 'https://example.test/v1'\n",
                "unrelated": { "keep": true }
            }),
            None,
        );
        let original_auth = provider.settings_config["auth"].clone();

        let updated = with_provider_model_catalog(
            &provider,
            &[CatalogModelInput {
                model: "GLM-5.2".into(),
                display_name: Some("GLM-5.2".into()),
                context_window: Some(200_000),
                ..Default::default()
            }],
        )
        .unwrap();

        assert_eq!(updated.settings_config["auth"], original_auth);
        assert_eq!(updated.settings_config["unrelated"]["keep"], true);
        assert_eq!(
            updated.settings_config["modelCatalog"]["models"][0]["model"],
            "GLM-5.2"
        );
        assert!(provider.settings_config.get("modelCatalog").is_none());

        let cleared = without_provider_model_catalog(&updated);
        assert!(cleared.settings_config.get("modelCatalog").is_none());
        assert_eq!(cleared.settings_config["auth"], original_auth);
    }

    #[test]
    fn takeover_repairs_empty_routes_from_the_only_configured_provider_catalog() {
        let db = Database::memory().unwrap();
        let provider = Provider::with_id(
            "weikuwu".into(),
            "weikuwu".into(),
            serde_json::json!({
                "modelCatalog": {"models": [
                    {"model":"GLM-5.2","displayName":"GLM-5.2"},
                    {"model":"GLM-5.2p","displayName":"GLM-5.2p"}
                ]}
            }),
            None,
        );
        db.save_provider("codex", &provider).unwrap();
        save(&db, &ModelRouteSettings::default()).unwrap();

        let repaired = ensure_configured_with_official_models(
            &db,
            vec![CatalogModelInput {
                model: "gpt-5.6-sol".into(),
                display_name: Some("GPT-5.6-Sol".into()),
                ..Default::default()
            }],
        )
        .unwrap();

        assert_eq!(repaired.official_models.len(), 1);
        assert_eq!(repaired.routes.len(), 2);
        assert_eq!(repaired.routes[0].upstream_model, "GLM-5.2");
        assert_eq!(
            db.get_setting(PROVIDER_KEY).unwrap().as_deref(),
            Some("weikuwu")
        );
    }

    #[test]
    fn builds_distinct_aliases_and_keeps_official_models() {
        let settings = build_settings(
            "provider-12345678",
            "Weikuwu",
            vec![CatalogModelInput {
                model: "gpt-5.6".into(),
                display_name: None,
                context_window: None,
                ..Default::default()
            }],
            vec![CatalogModelInput {
                model: "GLM-5.2p".into(),
                display_name: None,
                context_window: Some(200_000),
                ..Default::default()
            }],
        )
        .unwrap();
        assert_eq!(settings.official_models[0].model, "gpt-5.6");
        assert_eq!(settings.routes[0].alias, "ccs-provider-0-GLM-5.2p");
        assert_eq!(settings.routes[0].upstream_model, "GLM-5.2p");
    }

    #[test]
    fn resolves_unknown_models_to_official_while_enabled() {
        let db = Database::memory().unwrap();
        let settings = build_settings(
            "provider",
            "Third",
            vec![CatalogModelInput {
                model: "gpt-5.6".into(),
                display_name: None,
                context_window: None,
                ..Default::default()
            }],
            vec![CatalogModelInput {
                model: "glm".into(),
                display_name: None,
                context_window: None,
                ..Default::default()
            }],
        )
        .unwrap();
        save(&db, &settings).unwrap();
        assert_eq!(
            resolve(&db, "gpt-5.6").unwrap(),
            Some(ResolvedModelRoute::Official)
        );
        assert!(matches!(
            resolve(&db, &settings.routes[0].alias).unwrap(),
            Some(ResolvedModelRoute::ThirdParty(_))
        ));
    }

    #[test]
    fn augments_catalog_with_official_and_third_party_models() {
        let db = Database::memory().unwrap();
        let settings = build_settings(
            "provider",
            "Third",
            vec![CatalogModelInput {
                model: "gpt-5.6".into(),
                display_name: Some("GPT-5.6".into()),
                context_window: Some(272_000),
                ..Default::default()
            }],
            vec![CatalogModelInput {
                model: "glm".into(),
                display_name: Some("GLM".into()),
                context_window: Some(200_000),
                ..Default::default()
            }],
        )
        .unwrap();
        save(&db, &settings).unwrap();

        let output =
            augment_settings(&db, &serde_json::json!({ "config": "model = 'gpt-5.6'" })).unwrap();
        let models = output["modelCatalog"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "gpt-5.6");
        assert_eq!(models[1]["model"], settings.routes[0].alias);
        assert_eq!(models[1]["displayName"], "[Third] GLM");
        assert_eq!(models[0]["defaultReasoningLevel"], "medium");
        assert_eq!(models[1]["defaultReasoningLevel"], "high");
        assert_eq!(
            models[1]["supportedReasoningLevels"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|level| level["effort"].as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn augments_catalog_with_every_selected_third_party_model() {
        let db = Database::memory().unwrap();
        let selected = [
            "DeepSeek-V4-Flash",
            "GLM-5.2",
            "GLM-5.2p",
            "nemotron-3-ultra",
        ];
        let settings = build_settings(
            "provider",
            "weikuwu",
            vec![CatalogModelInput {
                model: "gpt-5.6-sol".into(),
                display_name: Some("GPT-5.6-Sol".into()),
                ..Default::default()
            }],
            selected
                .iter()
                .map(|model| CatalogModelInput {
                    model: (*model).into(),
                    display_name: Some((*model).into()),
                    ..Default::default()
                })
                .collect(),
        )
        .unwrap();
        save(&db, &settings).unwrap();

        let output = augment_settings(&db, &serde_json::json!({})).unwrap();
        let models = output["modelCatalog"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 1 + selected.len());
        for (index, expected) in selected.iter().enumerate() {
            assert_eq!(
                models[index + 1]["displayName"],
                format!("[weikuwu] {expected}")
            );
            assert!(models[index + 1]["model"]
                .as_str()
                .unwrap()
                .starts_with("ccs-provider-"));
        }
    }

    #[test]
    fn excludes_auto_review_and_stale_managed_aliases_from_official_catalog() {
        let settings = build_settings(
            "provider",
            "Third",
            vec![
                CatalogModelInput {
                    model: "gpt-5.6".into(),
                    display_name: Some("GPT-5.6".into()),
                    context_window: None,
                    ..Default::default()
                },
                CatalogModelInput {
                    model: super::super::auto_review::AUTO_REVIEW_MODEL.into(),
                    display_name: Some("Codex Auto Review".into()),
                    context_window: None,
                    ..Default::default()
                },
                CatalogModelInput {
                    model: "ccs-old-alias".into(),
                    display_name: Some("Old injected model".into()),
                    context_window: None,
                    ..Default::default()
                },
            ],
            vec![CatalogModelInput {
                model: "glm".into(),
                display_name: Some("GLM".into()),
                context_window: None,
                ..Default::default()
            }],
        )
        .unwrap();

        assert_eq!(settings.official_models.len(), 1);
        assert_eq!(settings.official_models[0].model, "gpt-5.6");
    }
}
