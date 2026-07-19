use crate::{database::Database, error::AppError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SETTINGS_KEY: &str = "codex_model_routes";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelInput {
    pub model: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
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
            if model.is_empty() || model.starts_with("ccs-") {
                return None;
            }
            Some(CatalogModelInput {
                model: model.to_string(),
                display_name: entry
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context_window: entry.get("context_window").and_then(Value::as_u64),
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
                "{provider_name} · {}",
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
        combined.push(serde_json::json!({
            "model": model.model,
            "displayName": model.display_name,
            "contextWindow": model.context_window,
        }));
    }
    for route in settings.routes {
        combined.push(serde_json::json!({
            "model": route.alias,
            "displayName": route.display_name,
            "contextWindow": route.context_window,
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
    fn builds_distinct_aliases_and_keeps_official_models() {
        let settings = build_settings(
            "provider-12345678",
            "Weikuwu",
            vec![CatalogModelInput {
                model: "gpt-5.6".into(),
                display_name: None,
                context_window: None,
            }],
            vec![CatalogModelInput {
                model: "GLM-5.2p".into(),
                display_name: None,
                context_window: Some(200_000),
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
            }],
            vec![CatalogModelInput {
                model: "glm".into(),
                display_name: None,
                context_window: None,
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
            }],
            vec![CatalogModelInput {
                model: "glm".into(),
                display_name: Some("GLM".into()),
                context_window: Some(200_000),
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
        assert_eq!(models[1]["displayName"], "Third · GLM");
    }
}
