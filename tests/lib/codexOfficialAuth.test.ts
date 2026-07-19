import { describe, expect, it } from "vitest";
import {
  resolveCodexOfficialSelection,
  updateCodexOfficialAuthMeta,
} from "@/lib/codexOfficialAuth";

describe("codex official auth selection", () => {
  it("keeps legacy providers on native auth", () => {
    expect(resolveCodexOfficialSelection(undefined)).toBe("native");
  });

  it("binds and resolves a fixed managed account", () => {
    const meta = updateCodexOfficialAuthMeta(undefined, "account:account-b");
    expect(meta.codexOfficialAuthMode).toBe("managed_account");
    expect(meta.authBinding).toEqual({
      source: "managed_account",
      authProvider: "codex_oauth",
      accountId: "account-b",
    });
    expect(resolveCodexOfficialSelection(meta)).toBe("account:account-b");
  });

  it("removes the Codex binding when returning to native", () => {
    const managed = updateCodexOfficialAuthMeta(undefined, "managed_default");
    const native = updateCodexOfficialAuthMeta(managed, "native");
    expect(native.codexOfficialAuthMode).toBe("native");
    expect(native.authBinding).toBeUndefined();
  });
});
