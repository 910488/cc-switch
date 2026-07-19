import type {
  CompactionSettings,
  CompactionSummaryTarget,
} from "@/types/proxy";

export const SUMMARY_INPUT_BUDGET_RANGE = {
  min: 4_000,
  max: 236_000,
  defaultValue: 236_000,
} as const;

export const SUMMARY_OUTPUT_TOKENS_RANGE = {
  min: 1_200,
  max: 12_000,
  defaultValue: 12_000,
} as const;

export const normalizeCompactionSettings = (
  settings: CompactionSettings,
): CompactionSettings => {
  const summaryProviderId = settings.summaryProviderId?.trim() || undefined;
  const summaryModel = summaryProviderId
    ? settings.summaryModel?.trim() || undefined
    : undefined;
  return {
    ...settings,
    summaryProviderId,
    summaryModel,
    summaryInputBudget: Math.round(settings.summaryInputBudget),
    summaryMaxOutputTokens: Math.round(settings.summaryMaxOutputTokens),
  };
};

export const validateCompactionSettings = (
  settings: CompactionSettings,
): "inputBudget" | "outputTokens" | null => {
  if (
    !Number.isFinite(settings.summaryInputBudget) ||
    settings.summaryInputBudget < SUMMARY_INPUT_BUDGET_RANGE.min ||
    settings.summaryInputBudget > SUMMARY_INPUT_BUDGET_RANGE.max
  ) {
    return "inputBudget";
  }
  if (
    !Number.isFinite(settings.summaryMaxOutputTokens) ||
    settings.summaryMaxOutputTokens < SUMMARY_OUTPUT_TOKENS_RANGE.min ||
    settings.summaryMaxOutputTokens > SUMMARY_OUTPUT_TOKENS_RANGE.max
  ) {
    return "outputTokens";
  }
  return null;
};

export const selectedSummaryTarget = (
  settings: CompactionSettings,
  targets: CompactionSummaryTarget[],
): CompactionSummaryTarget | undefined =>
  targets.find((target) => target.providerId === settings.summaryProviderId);

export const effectiveSummaryModel = (
  settings: CompactionSettings,
  target?: CompactionSummaryTarget,
): string | undefined =>
  settings.summaryProviderId
    ? settings.summaryModel?.trim() || target?.defaultModel
    : undefined;
