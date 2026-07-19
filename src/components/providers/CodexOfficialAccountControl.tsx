import { useMemo } from "react";
import { RotateCcw, ShieldCheck } from "lucide-react";
import { toast } from "sonner";
import type { CodexOfficialAuthMode, Provider } from "@/types";
import { useUpdateProviderMutation } from "@/lib/query";
import { useCodexOauth } from "@/components/providers/forms/hooks/useCodexOauth";
import CodexOauthQuotaFooter from "@/components/CodexOauthQuotaFooter";
import SubscriptionQuotaFooter from "@/components/SubscriptionQuotaFooter";
import {
  useCodexOauthResetCredits,
  useConsumeCodexOauthReset,
} from "@/lib/query/subscription";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  CODEX_OFFICIAL_ACCOUNT_PREFIX,
  CODEX_OFFICIAL_MANAGED_DEFAULT,
  CODEX_OFFICIAL_NATIVE,
  resolveCodexOfficialSelection,
  updateCodexOfficialAuthMeta,
} from "@/lib/codexOfficialAuth";

interface Props {
  provider: Provider;
  isCurrent: boolean;
  isProxyTakeover: boolean;
}

export function CodexOfficialAccountControl({
  provider,
  isCurrent,
  isProxyTakeover,
}: Props) {
  const { accounts, defaultAccountId, hasAnyAccount } = useCodexOauth();
  const updateProvider = useUpdateProviderMutation("codex");
  const configuredMode: CodexOfficialAuthMode =
    provider.meta?.codexOfficialAuthMode ?? CODEX_OFFICIAL_NATIVE;
  const boundAccountId =
    provider.meta?.authBinding?.source === "managed_account" &&
    provider.meta.authBinding.authProvider === "codex_oauth"
      ? provider.meta.authBinding.accountId
      : undefined;
  const selectedValue = resolveCodexOfficialSelection(provider.meta);
  const effectiveMode = isProxyTakeover
    ? configuredMode
    : CODEX_OFFICIAL_NATIVE;
  const effectiveAccountId = useMemo(() => {
    if (effectiveMode === "managed_account") return boundAccountId ?? null;
    if (effectiveMode === "managed_default") return defaultAccountId;
    return null;
  }, [boundAccountId, defaultAccountId, effectiveMode]);
  const effectiveAccount = accounts.find(
    (account) => account.id === effectiveAccountId,
  );
  const resetCredits = useCodexOauthResetCredits(
    effectiveAccountId ?? "",
    effectiveMode !== CODEX_OFFICIAL_NATIVE && Boolean(effectiveAccountId),
  );
  const consumeReset = useConsumeCodexOauthReset();
  const availableCredits = (resetCredits.data?.credits ?? []).filter(
    (credit) => credit.status === "available",
  );
  const firstCredit = availableCredits[0];

  const saveSelection = async (value: string) => {
    await updateProvider.mutateAsync({
      provider: {
        ...provider,
        meta: updateCodexOfficialAuthMeta(provider.meta, value),
      },
    });
  };

  const useReset = async () => {
    if (!effectiveAccountId || !firstCredit) return;
    if (
      !window.confirm(
        `確定要為 ${effectiveAccount?.login ?? effectiveAccountId} 使用 1 次 Reset？此操作無法復原。`,
      )
    ) {
      return;
    }
    try {
      const result = await consumeReset.mutateAsync({
        accountId: effectiveAccountId,
        creditId: firstCredit.id,
      });
      toast.success(
        result.code === "reset" ? "額度已重置" : `Reset 結果：${result.code}`,
      );
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  return (
    <div className="relative mt-3 border-t border-border/70 pt-3">
      <div className="flex flex-col gap-3 lg:flex-row lg:items-center lg:justify-between">
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <ShieldCheck className="h-4 w-4 flex-shrink-0 text-sky-500" />
          <span className="text-xs font-medium text-muted-foreground">
            帳號
          </span>
          <Select
            value={selectedValue}
            onValueChange={(value) => void saveSelection(value)}
            disabled={updateProvider.isPending}
          >
            <SelectTrigger className="h-8 max-w-sm text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={CODEX_OFFICIAL_NATIVE}>
                Codex 原生登入
              </SelectItem>
              <SelectItem
                value={CODEX_OFFICIAL_MANAGED_DEFAULT}
                disabled={!hasAnyAccount}
              >
                跟隨 CC Switch 預設帳號
              </SelectItem>
              {accounts.map((account) => (
                <SelectItem
                  key={account.id}
                  value={`${CODEX_OFFICIAL_ACCOUNT_PREFIX}${account.id}`}
                >
                  {account.login}
                  {account.id === defaultAccountId ? "（預設）" : ""}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {!isProxyTakeover && configuredMode !== CODEX_OFFICIAL_NATIVE && (
            <span className="text-[11px] text-amber-600">
              Proxy 未接管；目前實際使用 Codex 原生登入
            </span>
          )}
        </div>

        <div className="flex min-w-0 items-center gap-2">
          {effectiveMode === CODEX_OFFICIAL_NATIVE ? (
            <SubscriptionQuotaFooter
              appId="codex"
              inline={true}
              isCurrent={isCurrent}
              autoQueryInterval={0}
            />
          ) : (
            <>
              <CodexOauthQuotaFooter
                meta={{
                  authBinding: {
                    source: "managed_account",
                    authProvider: "codex_oauth",
                    ...(effectiveAccountId
                      ? { accountId: effectiveAccountId }
                      : {}),
                  },
                }}
                inline={true}
                isCurrent={isCurrent}
              />
              <span className="whitespace-nowrap text-[11px] text-amber-600">
                Reset {resetCredits.data?.availableCount ?? 0}
              </span>
              <Button
                type="button"
                size="sm"
                variant="outline"
                className="h-7 border-amber-500/50 px-2 text-[11px] text-amber-600"
                disabled={!firstCredit || consumeReset.isPending}
                onClick={() => void useReset()}
              >
                <RotateCcw className="mr-1 h-3 w-3" />
                使用 Reset
              </Button>
            </>
          )}
        </div>
      </div>
      {!hasAnyAccount && configuredMode === CODEX_OFFICIAL_NATIVE && (
        <p className="mt-2 text-[11px] text-muted-foreground">
          可到「設定 → 驗證 → ChatGPT (Codex OAuth)」新增託管帳號。
        </p>
      )}
    </div>
  );
}
