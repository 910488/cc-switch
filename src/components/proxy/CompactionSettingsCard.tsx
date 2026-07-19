import { useEffect, useMemo, useState } from "react";
import { BrainCircuit, Loader2, Save, ShieldCheck } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  useContinuitySettings,
  useContinuitySummaryTargets,
  useUpdateContinuitySettings,
} from "@/lib/query/proxy";
import type { CompactionRolloutMode, CompactionSettings } from "@/types/proxy";
import { extractErrorMessage } from "@/utils/errorUtils";
import {
  effectiveSummaryModel,
  normalizeCompactionSettings,
  selectedSummaryTarget,
  SUMMARY_INPUT_BUDGET_RANGE,
  SUMMARY_OUTPUT_TOKENS_RANGE,
  validateCompactionSettings,
} from "./compactionSettings";

interface CompactionSettingsCardProps {
  isProxyRunning: boolean;
}

export function CompactionSettingsCard({
  isProxyRunning,
}: CompactionSettingsCardProps) {
  const { t } = useTranslation();
  const { data: settings, isLoading: settingsLoading } =
    useContinuitySettings();
  const { data: targets = [], isLoading: targetsLoading } =
    useContinuitySummaryTargets();
  const updateSettings = useUpdateContinuitySettings();
  const [draft, setDraft] = useState<CompactionSettings | null>(null);

  useEffect(() => {
    if (!settings) return;
    setDraft({
      ...settings,
      summaryInputBudget:
        settings.summaryInputBudget ?? SUMMARY_INPUT_BUDGET_RANGE.defaultValue,
      summaryMaxOutputTokens:
        settings.summaryMaxOutputTokens ??
        SUMMARY_OUTPUT_TOKENS_RANGE.defaultValue,
    });
  }, [settings]);

  const selectedTarget = useMemo(
    () => (draft ? selectedSummaryTarget(draft, targets) : undefined),
    [draft, targets],
  );
  const effectiveModel = draft
    ? effectiveSummaryModel(draft, selectedTarget)
    : undefined;
  const selectedTargetMissing = Boolean(
    draft?.summaryProviderId && !selectedTarget && !targetsLoading,
  );

  const save = async () => {
    if (!draft) return;
    const normalized = normalizeCompactionSettings(draft);
    const invalid = validateCompactionSettings(normalized);
    if (invalid) {
      toast.error(
        t(`proxy.continuity.validation.${invalid}`, {
          min:
            invalid === "inputBudget"
              ? SUMMARY_INPUT_BUDGET_RANGE.min
              : SUMMARY_OUTPUT_TOKENS_RANGE.min,
          max:
            invalid === "inputBudget"
              ? SUMMARY_INPUT_BUDGET_RANGE.max
              : SUMMARY_OUTPUT_TOKENS_RANGE.max,
        }),
      );
      return;
    }
    if (normalized.summaryProviderId && !selectedTarget) {
      toast.error(t("proxy.continuity.validation.providerMissing"));
      return;
    }
    try {
      await updateSettings.mutateAsync(normalized);
      setDraft(normalized);
      toast.success(t("proxy.continuity.saved"));
    } catch (error) {
      toast.error(
        t("proxy.continuity.saveFailed", {
          error: extractErrorMessage(error),
        }),
      );
    }
  };

  if (settingsLoading || !draft) {
    return (
      <div className="flex min-h-32 items-center justify-center rounded-lg border border-border bg-card/60">
        <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  const modelOptions = selectedTarget?.models ?? [];

  return (
    <div className="rounded-lg border border-border bg-card/60 p-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="flex gap-3">
          <BrainCircuit className="mt-0.5 h-5 w-5 text-blue-500" />
          <div>
            <h4 className="text-sm font-semibold">
              {t("proxy.continuity.configurationTitle")}
            </h4>
            <p className="mt-1 text-xs text-muted-foreground">
              {t("proxy.continuity.configurationDescription")}
            </p>
          </div>
        </div>
        <Button
          type="button"
          size="sm"
          onClick={() => void save()}
          disabled={updateSettings.isPending || targetsLoading}
        >
          {updateSettings.isPending ? (
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
          ) : (
            <Save className="mr-2 h-4 w-4" />
          )}
          {t("common.save")}
        </Button>
      </div>

      <div className="mt-4 grid gap-4 border-t border-border pt-4 md:grid-cols-2">
        <div className="space-y-2">
          <Label htmlFor="continuity-rollout">
            {t("proxy.continuity.rolloutMode")}
          </Label>
          <select
            id="continuity-rollout"
            className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
            value={draft.rolloutMode}
            onChange={(event) =>
              setDraft((current) =>
                current
                  ? {
                      ...current,
                      rolloutMode: event.target.value as CompactionRolloutMode,
                    }
                  : current,
              )
            }
          >
            <option value="off">{t("proxy.continuity.rollout.off")}</option>
            <option value="observe-only">
              {t("proxy.continuity.rollout.observeOnly")}
            </option>
            <option value="third-party-only">
              {t("proxy.continuity.rollout.thirdPartyOnly")}
            </option>
            <option value="full-switching">
              {t("proxy.continuity.rollout.fullSwitching")}
            </option>
          </select>
          <p className="text-xs text-muted-foreground">
            {t("proxy.continuity.rolloutHint")}
          </p>
        </div>

        <div className="flex items-center justify-between gap-4 rounded-md border border-border bg-background/60 px-3 py-2">
          <div className="space-y-0.5">
            <Label className="flex items-center gap-2 text-sm font-medium">
              <ShieldCheck className="h-4 w-4" />
              {t("proxy.continuity.officialFallback")}
            </Label>
            <p className="text-xs text-muted-foreground">
              {t("proxy.continuity.officialFallbackHint")}
            </p>
          </div>
          <Switch
            checked={draft.officialCompactFallback}
            onCheckedChange={(checked) =>
              setDraft((current) =>
                current
                  ? { ...current, officialCompactFallback: checked }
                  : current,
              )
            }
          />
        </div>

        <div className="space-y-2">
          <Label htmlFor="continuity-summary-provider">
            {t("proxy.continuity.summaryProvider")}
          </Label>
          <select
            id="continuity-summary-provider"
            className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
            value={draft.summaryProviderId ?? ""}
            disabled={targetsLoading}
            onChange={(event) => {
              const providerId = event.target.value || undefined;
              const target = targets.find(
                (candidate) => candidate.providerId === providerId,
              );
              setDraft((current) =>
                current
                  ? {
                      ...current,
                      summaryProviderId: providerId,
                      summaryModel: providerId
                        ? target?.defaultModel
                        : undefined,
                    }
                  : current,
              );
            }}
          >
            <option value="">{t("proxy.continuity.followActiveRoute")}</option>
            {selectedTargetMissing && (
              <option value={draft.summaryProviderId} disabled>
                {t("proxy.continuity.missingProvider", {
                  id: draft.summaryProviderId,
                })}
              </option>
            )}
            {targets.map((target) => (
              <option key={target.providerId} value={target.providerId}>
                {target.providerName} · {target.protocol}
              </option>
            ))}
          </select>
          <p className="text-xs text-muted-foreground">
            {t("proxy.continuity.summaryProviderHint")}
          </p>
        </div>

        <div className="space-y-2">
          <Label htmlFor="continuity-summary-model">
            {t("proxy.continuity.summaryModel")}
          </Label>
          <Input
            id="continuity-summary-model"
            list="continuity-summary-model-options"
            value={draft.summaryModel ?? ""}
            disabled={!draft.summaryProviderId}
            placeholder={
              selectedTarget?.defaultModel ??
              t("proxy.continuity.summaryModelPlaceholder")
            }
            onChange={(event) =>
              setDraft((current) =>
                current
                  ? { ...current, summaryModel: event.target.value }
                  : current,
              )
            }
          />
          <datalist id="continuity-summary-model-options">
            {modelOptions.map((model) => (
              <option key={model} value={model} />
            ))}
          </datalist>
          <p className="text-xs text-muted-foreground">
            {t("proxy.continuity.summaryModelHint")}
          </p>
        </div>
      </div>

      <div
        className={`mt-4 rounded-md border px-3 py-2 text-xs ${
          selectedTargetMissing
            ? "border-destructive/50 bg-destructive/5 text-destructive"
            : "border-blue-500/30 bg-blue-500/5 text-muted-foreground"
        }`}
      >
        <span className="font-medium text-foreground">
          {t("proxy.continuity.effectiveTarget")}:
        </span>{" "}
        {selectedTarget
          ? `${selectedTarget.providerName} · ${
              effectiveModel ?? t("proxy.continuity.providerDefaultModel")
            }`
          : selectedTargetMissing
            ? t("proxy.continuity.missingProvider", {
                id: draft.summaryProviderId,
              })
            : t("proxy.continuity.activeRouteTarget")}
        {!isProxyRunning && (
          <span className="ml-1">
            {t("proxy.continuity.appliesWhenStarted")}
          </span>
        )}
      </div>

      <details className="mt-4 border-t border-border pt-3">
        <summary className="cursor-pointer text-xs font-medium text-foreground">
          {t("proxy.continuity.advancedLimits")}
        </summary>
        <p className="mt-1 text-xs text-muted-foreground">
          {t("proxy.continuity.advancedLimitsHint")}
        </p>
        <div className="mt-3 grid gap-4 md:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="continuity-input-budget">
              {t("proxy.continuity.inputBudget")}
            </Label>
            <Input
              id="continuity-input-budget"
              type="number"
              min={SUMMARY_INPUT_BUDGET_RANGE.min}
              max={SUMMARY_INPUT_BUDGET_RANGE.max}
              step={1_000}
              value={draft.summaryInputBudget}
              onChange={(event) =>
                setDraft((current) =>
                  current
                    ? {
                        ...current,
                        summaryInputBudget: Number(event.target.value),
                      }
                    : current,
                )
              }
            />
            <p className="text-xs text-muted-foreground">
              {t("proxy.continuity.inputBudgetHint", {
                min: SUMMARY_INPUT_BUDGET_RANGE.min.toLocaleString(),
                max: SUMMARY_INPUT_BUDGET_RANGE.max.toLocaleString(),
              })}
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="continuity-output-tokens">
              {t("proxy.continuity.outputTokens")}
            </Label>
            <Input
              id="continuity-output-tokens"
              type="number"
              min={SUMMARY_OUTPUT_TOKENS_RANGE.min}
              max={SUMMARY_OUTPUT_TOKENS_RANGE.max}
              step={100}
              value={draft.summaryMaxOutputTokens}
              onChange={(event) =>
                setDraft((current) =>
                  current
                    ? {
                        ...current,
                        summaryMaxOutputTokens: Number(event.target.value),
                      }
                    : current,
                )
              }
            />
            <p className="text-xs text-muted-foreground">
              {t("proxy.continuity.outputTokensHint", {
                min: SUMMARY_OUTPUT_TOKENS_RANGE.min.toLocaleString(),
                max: SUMMARY_OUTPUT_TOKENS_RANGE.max.toLocaleString(),
              })}
            </p>
          </div>
        </div>
      </details>
    </div>
  );
}
