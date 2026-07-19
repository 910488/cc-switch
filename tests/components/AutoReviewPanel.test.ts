import { describe, expect, it } from "vitest";
import type { Provider } from "@/types";
import {
  modeDescription,
  providerModels,
} from "@/components/proxy/AutoReviewPanel";

function provider(settingsConfig: Record<string, unknown>): Provider {
  return {
    id: "third-party",
    name: "Third party",
    settingsConfig,
  };
}

describe("AutoReviewPanel", () => {
  it("uses the selected provider model catalog", () => {
    expect(
      providerModels(
        provider({
          modelCatalog: {
            models: [
              { model: "GLM-5.2", displayName: "GLM" },
              { model: "", displayName: "invalid" },
            ],
          },
        }),
      ),
    ).toEqual([{ model: "GLM-5.2", displayName: "GLM" }]);
  });

  it("falls back to the provider TOML model", () => {
    expect(providerModels(provider({ config: 'model = "kimi-k2.7"' }))).toEqual(
      [{ model: "kimi-k2.7" }],
    );
  });

  it("describes auto mode as quota-only fallback", () => {
    expect(modeDescription("auto")).toContain("429");
    expect(modeDescription("always")).toContain("直接");
  });
});
