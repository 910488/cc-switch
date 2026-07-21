import type { CodexOfficialAuthMode, ProviderMeta } from "@/types";

export const CODEX_OFFICIAL_NATIVE = "native";
export const CODEX_OFFICIAL_MANAGED_DEFAULT = "managed_default";
export const CODEX_OFFICIAL_ACCOUNT_PREFIX = "account:";

export interface CodexOfficialQuotaSelection {
  mode: CodexOfficialAuthMode;
  accountId: string | null;
}

/**
 * Resolve which credentials own the quota shown in the official-provider row.
 *
 * This deliberately does not depend on proxy takeover. Takeover controls which
 * credentials requests use; the account picker must always preview the account
 * the user selected, even while takeover is off.
 */
export function resolveCodexOfficialQuotaSelection(
  meta: ProviderMeta | undefined,
  defaultAccountId: string | null,
): CodexOfficialQuotaSelection {
  const mode = meta?.codexOfficialAuthMode ?? CODEX_OFFICIAL_NATIVE;
  if (mode === "managed_default") {
    return { mode, accountId: defaultAccountId };
  }
  if (
    mode === "managed_account" &&
    meta?.authBinding?.source === "managed_account" &&
    meta.authBinding.authProvider === "codex_oauth"
  ) {
    return { mode, accountId: meta.authBinding.accountId ?? null };
  }
  return { mode: CODEX_OFFICIAL_NATIVE, accountId: null };
}

export function resolveCodexOfficialSelection(meta?: ProviderMeta): string {
  const mode = meta?.codexOfficialAuthMode ?? CODEX_OFFICIAL_NATIVE;
  const binding = meta?.authBinding;
  if (
    mode === "managed_account" &&
    binding?.source === "managed_account" &&
    binding.authProvider === "codex_oauth" &&
    binding.accountId
  ) {
    return `${CODEX_OFFICIAL_ACCOUNT_PREFIX}${binding.accountId}`;
  }
  return mode;
}

export function updateCodexOfficialAuthMeta(
  current: ProviderMeta | undefined,
  selection: string,
): ProviderMeta {
  let mode: CodexOfficialAuthMode;
  let accountId: string | undefined;
  if (selection.startsWith(CODEX_OFFICIAL_ACCOUNT_PREFIX)) {
    mode = "managed_account";
    accountId = selection.slice(CODEX_OFFICIAL_ACCOUNT_PREFIX.length);
  } else if (selection === CODEX_OFFICIAL_MANAGED_DEFAULT) {
    mode = "managed_default";
  } else {
    mode = "native";
  }

  const previousBinding = current?.authBinding;
  const authBinding =
    mode === "native"
      ? previousBinding?.authProvider === "codex_oauth"
        ? undefined
        : previousBinding
      : {
          source: "managed_account" as const,
          authProvider: "codex_oauth",
          ...(accountId ? { accountId } : {}),
        };

  return {
    ...current,
    codexOfficialAuthMode: mode,
    authBinding,
  };
}
