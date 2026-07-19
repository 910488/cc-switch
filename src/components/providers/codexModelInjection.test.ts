import { describe, expect, it } from "vitest";
import type { Provider } from "@/types";
import {
  buildCodexInjectedProvider,
  codexProviderConnection,
  configuredCodexModels,
} from "./codexModelInjection";

const provider: Provider = {
  id: "third-party",
  name: "Third party",
  category: "custom",
  settingsConfig: {
    auth: { OPENAI_API_KEY: "secret" },
    config:
      'model = "old-model"\n[model_providers.custom]\nbase_url = "https://example.test/v1"\n',
  },
  meta: { isFullUrl: true },
};

describe("Codex model injection", () => {
  it("extracts the provider connection without exposing it in the catalog", () => {
    expect(codexProviderConnection(provider)).toEqual({
      baseUrl: "https://example.test/v1",
      apiKey: "secret",
      configuredAsFullUrl: true,
    });
    expect(configuredCodexModels(provider)).toEqual([
      { model: "old-model", displayName: "old-model" },
    ]);
  });

  it("writes selected models and repairs a /v1 URL misclassified as full URL", () => {
    const updated = buildCodexInjectedProvider(provider, [
      { model: "GLM-5.2p" },
      { model: "GLM-5.2" },
    ]);
    expect(updated.meta?.isFullUrl).toBe(false);
    expect(updated.settingsConfig.modelCatalog.models).toHaveLength(2);
    expect(updated.settingsConfig.config).toContain('model = "GLM-5.2p"');
  });

  it("rejects an empty injected catalog", () => {
    expect(() => buildCodexInjectedProvider(provider, [])).toThrow(
      "At least one model",
    );
  });
});
