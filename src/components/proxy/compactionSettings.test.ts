import { describe, expect, it } from "vitest";
import type {
  CompactionSettings,
  CompactionSummaryTarget,
} from "@/types/proxy";
import {
  effectiveSummaryModel,
  normalizeCompactionSettings,
  selectedSummaryTarget,
  validateCompactionSettings,
} from "./compactionSettings";

const settings = (
  overrides: Partial<CompactionSettings> = {},
): CompactionSettings => ({
  rolloutMode: "full-switching",
  officialCompactFallback: true,
  summaryInputBudget: 236_000,
  summaryMaxOutputTokens: 12_000,
  ...overrides,
});

const target: CompactionSummaryTarget = {
  providerId: "third-party-a",
  providerName: "Third Party A",
  protocol: "openai_chat",
  models: ["summary-small", "summary-large"],
  defaultModel: "summary-small",
};

describe("compaction settings", () => {
  it("removes a model when routing follows the active provider", () => {
    expect(
      normalizeCompactionSettings(
        settings({ summaryProviderId: "  ", summaryModel: "orphan-model" }),
      ),
    ).toMatchObject({ summaryProviderId: undefined, summaryModel: undefined });
  });

  it("keeps a trimmed pinned provider and exact model", () => {
    expect(
      normalizeCompactionSettings(
        settings({
          summaryProviderId: " third-party-a ",
          summaryModel: " summary-large ",
        }),
      ),
    ).toMatchObject({
      summaryProviderId: "third-party-a",
      summaryModel: "summary-large",
    });
  });

  it("validates only the executor limits exposed by the UI", () => {
    expect(validateCompactionSettings(settings())).toBeNull();
    expect(
      validateCompactionSettings(settings({ summaryInputBudget: 3_999 })),
    ).toBe("inputBudget");
    expect(
      validateCompactionSettings(settings({ summaryMaxOutputTokens: 12_001 })),
    ).toBe("outputTokens");
  });

  it("describes the effective pinned target and provider default model", () => {
    const pinned = settings({ summaryProviderId: target.providerId });
    expect(selectedSummaryTarget(pinned, [target])).toEqual(target);
    expect(effectiveSummaryModel(pinned, target)).toBe("summary-small");
    expect(
      effectiveSummaryModel(
        { ...pinned, summaryModel: "summary-large" },
        target,
      ),
    ).toBe("summary-large");
  });
});
