import { Activity, Coins, Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { ProviderIcon } from "@/components/ProviderIcon";
import { useProviderStats } from "@/lib/query/usage";
import type { UsageRangeSelection } from "@/types/usage";
import { fmtUsd, formatTokensShort, getResolvedLang } from "./format";

interface ProviderUsageBreakdownProps {
  range: UsageRangeSelection;
  appType?: string;
  providerName?: string;
  model?: string;
  refreshIntervalMs: number;
}

function providerIcon(
  providerId: string,
  providerName: string,
): string | undefined {
  const identity = `${providerId} ${providerName}`.toLowerCase();
  if (identity.includes("openai") || identity.includes("official"))
    return "openai";
  if (identity.includes("grok") || identity.includes("xai")) return "grok";
  if (identity.includes("claude") || identity.includes("anthropic"))
    return "claude";
  if (identity.includes("gemini") || identity.includes("google"))
    return "gemini";
  return undefined;
}

export function ProviderUsageBreakdown({
  range,
  appType,
  providerName,
  model,
  refreshIntervalMs,
}: ProviderUsageBreakdownProps) {
  const { t, i18n } = useTranslation();
  const language = getResolvedLang(i18n);
  const { data: stats = [], isLoading } = useProviderStats(
    range,
    { appType, providerName, model },
    { refetchInterval: refreshIntervalMs > 0 ? refreshIntervalMs : false },
  );

  if (isLoading) {
    return (
      <div className="flex h-28 items-center justify-center rounded-xl border border-border/50 bg-card/40">
        <Loader2 className="h-6 w-6 animate-spin text-muted-foreground/40" />
      </div>
    );
  }

  if (stats.length === 0) return null;

  const sorted = [...stats].sort(
    (left, right) => right.totalTokens - left.totalTokens,
  );

  return (
    <section className="space-y-3 rounded-xl border border-border/50 bg-card/40 p-4 backdrop-blur-sm">
      <div>
        <h3 className="text-sm font-semibold">
          {t("usage.providerBreakdown.title", "Usage by provider")}
        </h3>
        <p className="text-xs text-muted-foreground">
          {t(
            "usage.providerBreakdown.description",
            "Native Codex sessions are counted with OpenAI Official; matching session and proxy records are counted once.",
          )}
        </p>
      </div>

      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
        {sorted.map((stat) => {
          const icon = providerIcon(stat.providerId, stat.providerName);
          return (
            <div
              key={stat.providerId}
              className="rounded-xl border border-border/40 bg-background/45 p-3 shadow-sm"
            >
              <div className="flex items-center gap-2.5">
                <ProviderIcon
                  icon={icon}
                  name={stat.providerName}
                  size={28}
                  className="rounded-lg"
                />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">
                    {stat.providerName}
                  </div>
                  <div
                    className="text-lg font-bold tabular-nums"
                    title={stat.totalTokens.toLocaleString()}
                  >
                    {formatTokensShort(stat.totalTokens, language, 2)}
                    <span className="ml-1 text-[10px] font-medium uppercase text-muted-foreground">
                      Token
                    </span>
                  </div>
                </div>
              </div>

              <div className="mt-3 flex items-center justify-between text-xs text-muted-foreground">
                <span className="flex items-center gap-1">
                  <Activity className="h-3.5 w-3.5" />
                  {stat.requestCount.toLocaleString()}{" "}
                  {t("usage.requests", "requests")}
                </span>
                <span className="flex items-center gap-1 text-emerald-500">
                  <Coins className="h-3.5 w-3.5" />
                  {fmtUsd(stat.totalCost, 4)}
                </span>
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}
