import type { CodexCatalogModel, Provider } from "@/types";
import {
  extractCodexBaseUrl,
  extractCodexExperimentalBearerToken,
  extractCodexModelName,
  setCodexModelName,
} from "@/utils/providerConfigUtils";

export interface CodexProviderConnection {
  baseUrl: string;
  apiKey: string;
  configuredAsFullUrl: boolean;
}

export function codexProviderConnection(
  provider: Provider,
): CodexProviderConnection {
  const configText =
    typeof provider.settingsConfig?.config === "string"
      ? provider.settingsConfig.config
      : "";
  const auth = provider.settingsConfig?.auth;
  const fallbackBaseUrl = configText.match(
    /^\s*base_url\s*=\s*["']([^"']+)["']/m,
  )?.[1];
  const apiKey =
    typeof auth?.OPENAI_API_KEY === "string" && auth.OPENAI_API_KEY.trim()
      ? auth.OPENAI_API_KEY.trim()
      : extractCodexExperimentalBearerToken(configText) || "";
  return {
    baseUrl: extractCodexBaseUrl(configText) || fallbackBaseUrl || "",
    apiKey,
    configuredAsFullUrl: provider.meta?.isFullUrl === true,
  };
}

export function configuredCodexModels(provider: Provider): CodexCatalogModel[] {
  const models = provider.settingsConfig?.modelCatalog?.models;
  if (Array.isArray(models) && models.length > 0) {
    return models.filter((entry): entry is CodexCatalogModel =>
      Boolean(entry && typeof entry.model === "string" && entry.model.trim()),
    );
  }
  const configText =
    typeof provider.settingsConfig?.config === "string"
      ? provider.settingsConfig.config
      : "";
  const model = extractCodexModelName(configText);
  return model ? [{ model, displayName: model }] : [];
}

export function buildCodexInjectedProvider(
  provider: Provider,
  selectedModels: CodexCatalogModel[],
): Provider {
  const normalized = selectedModels
    .map((entry) => ({
      ...entry,
      model: entry.model.trim(),
      displayName: entry.displayName?.trim() || entry.model.trim(),
    }))
    .filter((entry) => entry.model);
  if (normalized.length === 0) {
    throw new Error("At least one model must be selected");
  }
  const configText =
    typeof provider.settingsConfig?.config === "string"
      ? provider.settingsConfig.config
      : "";
  const connection = codexProviderConnection(provider);
  const baseLooksLikePrefix = /\/v\d+\/?$/i.test(connection.baseUrl);

  return {
    ...provider,
    settingsConfig: {
      ...provider.settingsConfig,
      config: setCodexModelName(configText, normalized[0].model),
      modelCatalog: { models: normalized },
    },
    meta: {
      ...provider.meta,
      // A /v1-style URL is a base prefix, not a complete Responses endpoint.
      isFullUrl:
        connection.configuredAsFullUrl && !baseLooksLikePrefix ? true : false,
    },
  };
}
