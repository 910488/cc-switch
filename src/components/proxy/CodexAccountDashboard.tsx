import { useState } from "react";
import { Check, RefreshCw, RotateCcw, UserRound } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { useCodexOauth } from "@/components/providers/forms/hooks/useCodexOauth";
import {
  useCodexOauthQuotaByAccount,
  useCodexOauthResetCredits,
  useConsumeCodexOauthReset,
} from "@/lib/query/subscription";
import type { ManagedAuthAccount } from "@/lib/api/auth";

function formatReset(value: string | null | undefined): string {
  if (!value) return "–";
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return "–";
  return date.toLocaleString();
}

function AccountCard({
  account,
  active,
  switching,
  onSwitch,
}: {
  account: ManagedAuthAccount;
  active: boolean;
  switching: boolean;
  onSwitch: () => void;
}) {
  const quota = useCodexOauthQuotaByAccount(account.id, {
    enabled: true,
    autoQuery: true,
  });
  const resetCredits = useCodexOauthResetCredits(account.id, true);
  const consumeReset = useConsumeCodexOauthReset();
  const [refreshing, setRefreshing] = useState(false);
  const availableCredits = (resetCredits.data?.credits ?? []).filter(
    (credit) => credit.status === "available",
  );
  const firstCredit = availableCredits[0];

  const refresh = async () => {
    setRefreshing(true);
    try {
      await Promise.all([quota.refetch(), resetCredits.refetch()]);
    } finally {
      setRefreshing(false);
    }
  };

  const reset = async () => {
    if (!firstCredit) return;
    const title = firstCredit.title ?? "Full reset";
    if (
      !window.confirm(
        `${title}\n\n這會立即消耗 1 次 Reset 並重置此帳號的適用額度窗口。確定繼續？`,
      )
    ) {
      return;
    }
    try {
      const result = await consumeReset.mutateAsync({
        accountId: account.id,
        creditId: firstCredit.id,
      });
      const messages: Record<string, string> = {
        reset: `額度已重置。重置窗口數：${result.windowsReset ?? "–"}`,
        nothing_to_reset: "目前沒有需要重置的額度窗口，Reset 未使用。",
        no_credit: "目前沒有可用 Reset。",
        already_redeemed: "這次 Reset 已經使用過。",
      };
      toast.success(messages[result.code] ?? `Reset 回應：${result.code}`);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  return (
    <article
      className={`rounded-xl border p-4 ${
        active ? "border-primary bg-primary/5" : "border-border bg-card/60"
      }`}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <UserRound className="h-4 w-4 text-muted-foreground" />
            <span className="truncate text-sm font-semibold">
              {account.login}
            </span>
            {active && <Badge>預設</Badge>}
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            ChatGPT account · {account.id.slice(0, 12)}…
          </p>
        </div>
        <div className="flex gap-1">
          <Button
            type="button"
            size="icon"
            variant="ghost"
            onClick={() => void refresh()}
            disabled={refreshing}
            title="重新查詢配額"
          >
            <RefreshCw
              className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`}
            />
          </Button>
          <Button
            type="button"
            size="sm"
            variant={active ? "secondary" : "outline"}
            disabled={active || switching}
            onClick={onSwitch}
          >
            {active ? <Check className="mr-1 h-4 w-4" /> : null}
            {active ? "目前預設" : "設為預設"}
          </Button>
        </div>
      </div>

      <div className="mt-4 space-y-3">
        {quota.isLoading && (
          <p className="text-xs text-muted-foreground">正在查詢額度…</p>
        )}
        {(quota.data?.tiers ?? []).map((tier) => {
          const remaining = Math.max(0, Math.min(100, 100 - tier.utilization));
          const label =
            tier.name === "five_hour"
              ? "5 小時"
              : tier.name === "seven_day"
                ? "7 天"
                : tier.name === "30_day"
                  ? "30 天"
                  : tier.name;
          return (
            <div key={tier.name}>
              <div className="mb-1 flex justify-between gap-3 text-xs text-muted-foreground">
                <span>
                  {label} · 剩餘 {remaining.toFixed(0)}%
                </span>
                <span>重置 {formatReset(tier.resetsAt)}</span>
              </div>
              <div className="h-2 overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full bg-primary transition-all"
                  style={{ width: `${remaining}%` }}
                />
              </div>
            </div>
          );
        })}
        {quota.data && !quota.data.success && (
          <p className="text-xs text-destructive">
            {quota.data.error ?? quota.data.credentialMessage ?? "配額查詢失敗"}
          </p>
        )}
      </div>

      <div className="mt-4 flex items-center justify-between gap-3 border-t border-border pt-3">
        <div>
          <p className="text-xs font-semibold text-amber-600">
            Reset 可用{" "}
            {resetCredits.data?.availableCount ?? availableCredits.length} 次
          </p>
          <p className="mt-0.5 text-[11px] text-muted-foreground">
            {resetCredits.isError
              ? "Reset 查詢失敗，請重新整理"
              : firstCredit?.expiresAt
                ? `最近到期 ${formatReset(firstCredit.expiresAt)}`
                : "目前沒有可用 Reset"}
          </p>
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          className="border-amber-500/50 text-amber-600"
          disabled={!firstCredit || consumeReset.isPending}
          onClick={() => void reset()}
        >
          <RotateCcw className="mr-1 h-4 w-4" />
          使用 Reset
        </Button>
      </div>
    </article>
  );
}

export function CodexAccountDashboard() {
  const {
    accounts,
    defaultAccountId,
    hasAnyAccount,
    setDefaultAccount,
    isSettingDefaultAccount,
  } = useCodexOauth();

  if (!hasAnyAccount) {
    return (
      <div className="rounded-xl border border-dashed border-border p-4 text-sm text-muted-foreground">
        請先到「設定 → 驗證 → ChatGPT (Codex OAuth)」登入帳號。
      </div>
    );
  }

  return (
    <section className="space-y-3">
      <div>
        <h4 className="text-sm font-semibold">ChatGPT 帳號</h4>
        <p className="text-xs text-muted-foreground">
          手動切換預設帳號；未個別綁定帳號的 Codex OAuth provider 會立即使用它。
        </p>
      </div>
      <div className="grid gap-3 xl:grid-cols-2">
        {accounts.map((account) => (
          <AccountCard
            key={account.id}
            account={account}
            active={defaultAccountId === account.id}
            switching={isSettingDefaultAccount}
            onSwitch={() => setDefaultAccount(account.id)}
          />
        ))}
      </div>
    </section>
  );
}
