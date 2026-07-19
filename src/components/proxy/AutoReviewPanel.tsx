import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, RefreshCw, Save, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  useAutoReviewSettings,
  useAutoReviewStats,
  useUpdateAutoReviewSettings,
} from "@/lib/query/autoReview";
import { useProvidersQuery } from "@/lib/query/queries";
import type { CodexCatalogModel, Provider } from "@/types";
import type { AutoReviewMode, AutoReviewSettings } from "@/types/autoReview";
import { extractErrorMessage } from "@/utils/errorUtils";
import { extractCodexModelName } from "@/utils/providerConfigUtils";

const EMPTY_SETTINGS: AutoReviewSettings = {
  mode: "off",
  fallbackProviderId: "",
  fallbackModel: "",
  fallbackEffort: "none",
};

export function providerModels(
  provider: Provider | undefined,
): CodexCatalogModel[] {
  if (!provider) return [];
  const catalog = provider.settingsConfig?.modelCatalog?.models;
  const models = Array.isArray(catalog)
    ? catalog.filter((entry): entry is CodexCatalogModel =>
        Boolean(entry && typeof entry.model === "string" && entry.model.trim()),
      )
    : [];
  if (models.length > 0) return models;
  const config = provider.settingsConfig?.config;
  const fallback =
    typeof config === "string"
      ? extractCodexModelName(config)
      : typeof provider.settingsConfig?.model === "string"
        ? provider.settingsConfig.model
        : undefined;
  return fallback ? [{ model: fallback }] : [];
}

export function modeDescription(mode: AutoReviewMode): string {
  if (mode === "auto") {
    return "先使用 OpenAI 官方 codex-auto-review；只有官方回覆額度耗盡 429 時，才改用指定模型。";
  }
  if (mode === "always") {
    return "所有 codex-auto-review 請求直接送到指定的第三方 Provider 與模型。";
  }
  return "維持 Codex 原本路由，不啟用 Auto Review 替代模型。";
}

export function AutoReviewPanel({ proxyRunning }: { proxyRunning: boolean }) {
  const { data: providerData, isLoading: providersLoading } = useProvidersQuery(
    "codex",
    { isProxyRunning: proxyRunning },
  );
  const { data: stored, isLoading } = useAutoReviewSettings();
  const updateSettings = useUpdateAutoReviewSettings();
  const stats = useAutoReviewStats(proxyRunning);
  const [draft, setDraft] = useState<AutoReviewSettings>(EMPTY_SETTINGS);

  useEffect(() => {
    if (stored) setDraft(stored);
  }, [stored]);

  const providers = useMemo(
    () =>
      Object.values(providerData?.providers ?? {}).filter(
        (provider) =>
          provider.id !== "codex-official" && provider.category !== "official",
      ),
    [providerData?.providers],
  );
  const officialProvider = providerData?.providers?.["codex-official"];
  const officialUsesNativeAuth =
    (officialProvider?.meta?.codexOfficialAuthMode ?? "native") === "native";
  const selectedProvider = providers.find(
    (provider) => provider.id === draft.fallbackProviderId,
  );
  const models = useMemo(
    () => providerModels(selectedProvider),
    [selectedProvider],
  );
  const targetRequired = draft.mode !== "off";
  const targetValid = Boolean(selectedProvider && draft.fallbackModel.trim());
  const configuredProviderMissing = Boolean(
    draft.fallbackProviderId && !selectedProvider && !providersLoading,
  );

  const selectProvider = (providerId: string) => {
    const provider = providers.find((entry) => entry.id === providerId);
    setDraft((current) => ({
      ...current,
      fallbackProviderId: providerId,
      fallbackModel: providerModels(provider)[0]?.model ?? "",
    }));
  };

  const save = async () => {
    if (targetRequired && !targetValid) {
      toast.error(
        "啟用 Auto Review 替代時，請選擇第三方 Provider 並填入模型。",
      );
      return;
    }
    try {
      const saved = await updateSettings.mutateAsync({
        ...draft,
        fallbackModel: draft.fallbackModel.trim(),
      });
      if (saved) setDraft(saved);
      toast.success("Auto Review 設定已儲存");
    } catch (error) {
      toast.error(extractErrorMessage(error));
    }
  };

  if (isLoading) {
    return (
      <p className="text-sm text-muted-foreground">載入 Auto Review 設定…</p>
    );
  }

  return (
    <section className="space-y-4 rounded-lg border border-border bg-card/60 p-4">
      <div className="flex items-start justify-between gap-4">
        <div className="flex gap-3">
          <ShieldCheck className="mt-0.5 h-5 w-5 text-sky-500" />
          <div>
            <h4 className="text-sm font-semibold">Auto Review 替代模型</h4>
            <p className="mt-1 text-xs text-muted-foreground">
              為 Codex Guardian 的程式碼審查指定額度耗盡時使用的第三方模型。
            </p>
          </div>
        </div>
        <Button
          type="button"
          size="sm"
          onClick={() => void save()}
          disabled={
            updateSettings.isPending || (targetRequired && !targetValid)
          }
        >
          <Save className="mr-1.5 h-4 w-4" />
          儲存
        </Button>
      </div>

      <div className="grid gap-4 border-t border-border pt-4 md:grid-cols-2">
        <div className="space-y-2">
          <Label htmlFor="auto-review-mode">路由模式</Label>
          <Select
            value={draft.mode}
            onValueChange={(value) =>
              setDraft((current) => ({
                ...current,
                mode: value as AutoReviewMode,
              }))
            }
          >
            <SelectTrigger id="auto-review-mode">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="off">Off（維持原路由）</SelectItem>
              <SelectItem value="auto">Auto（官方額度 429 才替代）</SelectItem>
              <SelectItem value="always">Always（直接使用替代模型）</SelectItem>
            </SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">
            {modeDescription(draft.mode)}
          </p>
        </div>

        <div className="space-y-2">
          <Label htmlFor="auto-review-provider">替代 Provider</Label>
          <Select
            value={draft.fallbackProviderId || undefined}
            onValueChange={selectProvider}
            disabled={providersLoading || providers.length === 0}
          >
            <SelectTrigger id="auto-review-provider">
              <SelectValue placeholder="選擇第三方 Codex Provider" />
            </SelectTrigger>
            <SelectContent>
              {providers.map((provider) => (
                <SelectItem key={provider.id} value={provider.id}>
                  {provider.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {providers.length === 0 && (
            <p className="text-xs text-amber-600">
              請先在 Codex Provider 頁面新增第三方 Provider。
            </p>
          )}
          {configuredProviderMissing && (
            <p className="text-xs text-destructive">
              原先設定的 Provider 已不存在，請重新選擇並儲存。
            </p>
          )}
        </div>

        <div className="space-y-2">
          <Label htmlFor="auto-review-model">替代模型</Label>
          <Input
            id="auto-review-model"
            list="auto-review-model-options"
            value={draft.fallbackModel}
            onChange={(event) =>
              setDraft((current) => ({
                ...current,
                fallbackModel: event.target.value,
              }))
            }
            placeholder="例如 GLM-5.2"
            disabled={!draft.fallbackProviderId}
          />
          <datalist id="auto-review-model-options">
            {models.map((model) => (
              <option key={model.model} value={model.model}>
                {model.displayName ?? model.model}
              </option>
            ))}
          </datalist>
          <p className="text-xs text-muted-foreground">
            可從 Provider 的模型清單選取，也可直接輸入上游模型 ID。
          </p>
        </div>

        <div className="space-y-2">
          <Label htmlFor="auto-review-effort">Reasoning effort</Label>
          <Select
            value={draft.fallbackEffort || "none"}
            onValueChange={(value) =>
              setDraft((current) => ({ ...current, fallbackEffort: value }))
            }
          >
            <SelectTrigger id="auto-review-effort">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {["none", "minimal", "low", "medium", "high", "xhigh", "max"].map(
                (effort) => (
                  <SelectItem key={effort} value={effort}>
                    {effort}
                  </SelectItem>
                ),
              )}
            </SelectContent>
          </Select>
        </div>
      </div>

      {draft.mode === "auto" && officialUsesNativeAuth && (
        <div className="flex gap-2 rounded-md bg-amber-500/10 p-3 text-xs text-amber-700 dark:text-amber-400">
          <AlertTriangle className="h-4 w-4 flex-shrink-0" />
          <span>
            Auto 模式會先呼叫 OpenAI Official。若 Codex 沒有傳入可用的登入
            token，請在 OpenAI Official 使用 CC Switch 管理的 ChatGPT 帳號。
          </span>
        </div>
      )}

      <div className="grid gap-3 border-t border-border pt-4 sm:grid-cols-2 xl:grid-cols-5">
        <Stat label="官方嘗試" value={stats.data?.officialAttempts ?? 0} />
        <Stat label="官方額度 429" value={stats.data?.officialQuota429 ?? 0} />
        <Stat label="替代次數" value={stats.data?.fallbacks ?? 0} />
        <Stat label="替代成功" value={stats.data?.successfulFallbacks ?? 0} />
        <Stat label="替代失敗" value={stats.data?.failedFallbacks ?? 0} />
      </div>

      <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
        <span>
          最近替代：
          {stats.data?.lastFallbackAt
            ? new Date(stats.data.lastFallbackAt).toLocaleString()
            : "尚未發生"}
          {stats.data?.lastFallbackProviderId
            ? ` · ${stats.data.lastFallbackProviderId}/${stats.data.lastFallbackModel ?? "default"}`
            : ""}
        </span>
        <Button
          type="button"
          size="sm"
          variant="ghost"
          className="h-7"
          onClick={() => void stats.refetch()}
          disabled={stats.isFetching}
        >
          <RefreshCw
            className={`mr-1 h-3.5 w-3.5 ${stats.isFetching ? "animate-spin" : ""}`}
          />
          重新整理
        </Button>
      </div>

      {stats.data?.lastError && (
        <div className="flex gap-2 rounded-md bg-destructive/10 p-3 text-xs text-destructive">
          <AlertTriangle className="h-4 w-4 flex-shrink-0" />
          <span>{stats.data.lastError}</span>
        </div>
      )}
    </section>
  );
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-md bg-muted/60 px-3 py-2">
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p className="mt-0.5 text-lg font-semibold tabular-nums">{value}</p>
    </div>
  );
}
