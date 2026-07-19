import type { CodexOfficialAuthMode, ProviderMeta } from "@/types";

export const CODEX_OFFICIAL_NATIVE = "native";
export const CODEX_OFFICIAL_MANAGED_DEFAULT = "managed_default";
export const CODEX_OFFICIAL_ACCOUNT_PREFIX = "account:";

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
