import { useState } from "react";
import { KeyRound, Loader2, Plus, ShieldAlert, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  useDeleteProviderCredential,
  useProviderCredentials,
  useSaveProviderCredential,
} from "@/lib/query/proxy";
import { proxyApi } from "@/lib/api/proxy";
import type { ProviderCredential, ProviderCredentialKind } from "@/types/proxy";
import { extractErrorMessage } from "@/utils/errorUtils";

interface CredentialPoolSectionProps {
  appType: string;
  providerId: string;
}

export function CredentialPoolSection({
  appType,
  providerId,
}: CredentialPoolSectionProps) {
  const { data, isLoading, refetch } = useProviderCredentials(
    appType,
    providerId,
  );
  const saveCredential = useSaveProviderCredential();
  const deleteCredential = useDeleteProviderCredential();
  const [label, setLabel] = useState("");
  const [secret, setSecret] = useState("");
  const [kind, setKind] = useState<ProviderCredentialKind>("api_key");
  const [header, setHeader] = useState("authorization");
  const [prefix, setPrefix] = useState("Bearer ");
  const [priority, setPriority] = useState("100");

  const addCredential = async () => {
    if (!label.trim() || !secret) {
      toast.error("請輸入帳號名稱與 credential");
      return;
    }
    try {
      await saveCredential.mutateAsync({
        appType,
        providerId,
        kind,
        label: label.trim(),
        secret,
        enabled: true,
        priority: Number(priority) || 100,
        authHeader: header.trim() || "authorization",
        authPrefix: prefix,
        publicMetadata: {},
      });
      setLabel("");
      setSecret("");
      toast.success("Credential 已安全存入系統 vault");
    } catch (error) {
      toast.error(extractErrorMessage(error));
    }
  };

  const toggleCredential = async (
    credential: ProviderCredential,
    enabled: boolean,
  ) => {
    try {
      await proxyApi.updateProviderCredentialStatus(
        credential.id,
        enabled,
        credential.status,
        credential.lastErrorCode ?? undefined,
      );
      await refetch();
    } catch (error) {
      toast.error(extractErrorMessage(error));
    }
  };

  const removeCredential = async (credentialId: string) => {
    try {
      await deleteCredential.mutateAsync({
        appType,
        providerId,
        credentialId,
      });
      toast.success("Credential 已刪除");
    } catch (error) {
      toast.error(extractErrorMessage(error));
    }
  };

  return (
    <section className="space-y-4 rounded-lg border border-border p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="flex gap-3">
          <KeyRound className="mt-0.5 h-5 w-5 text-primary" />
          <div>
            <h3 className="text-sm font-semibold">多帳號 Credential Pool</h3>
            <p className="mt-1 text-xs text-muted-foreground">
              每次請求依 quota、優先序與最近使用時間選擇帳號；429
              只鎖定耗盡的帳號，不會停用整個 Provider。
            </p>
          </div>
        </div>
        {isLoading && <Loader2 className="h-4 w-4 animate-spin" />}
      </div>

      {data && !data.vaultAvailable && (
        <div className="flex gap-2 rounded-md border border-destructive/40 bg-destructive/5 p-3 text-xs text-destructive">
          <ShieldAlert className="h-4 w-4 shrink-0" />
          系統安全 vault 不可用；既有 Provider credential
          仍可使用，但無法新增帳號。
        </div>
      )}

      <div className="space-y-2">
        {data?.credentials.map((credential) => (
          <CredentialRow
            key={credential.id}
            credential={credential}
            busy={deleteCredential.isPending}
            onToggle={(enabled) => void toggleCredential(credential, enabled)}
            onDelete={() => void removeCredential(credential.id)}
          />
        ))}
        {data && data.credentials.length === 0 && (
          <p className="rounded-md bg-muted/50 p-3 text-xs text-muted-foreground">
            尚未建立 pool；目前繼續使用 Provider 原本的單一 credential。
          </p>
        )}
      </div>

      <div className="grid gap-3 border-t border-border pt-4 md:grid-cols-2">
        <div className="space-y-1">
          <Label htmlFor={"credential-label-" + providerId}>帳號名稱</Label>
          <Input
            id={"credential-label-" + providerId}
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            placeholder="例如：Work、Personal"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={"credential-kind-" + providerId}>類型</Label>
          <select
            id={"credential-kind-" + providerId}
            className="h-10 w-full rounded-md border border-input bg-background px-3 text-sm"
            value={kind}
            onChange={(event) =>
              setKind(event.target.value as ProviderCredentialKind)
            }
          >
            <option value="api_key">API key</option>
            <option value="token">Bearer token</option>
            <option value="oauth">OAuth token</option>
          </select>
        </div>
        <div className="space-y-1 md:col-span-2">
          <Label htmlFor={"credential-secret-" + providerId}>Credential</Label>
          <Input
            id={"credential-secret-" + providerId}
            type="password"
            autoComplete="new-password"
            value={secret}
            onChange={(event) => setSecret(event.target.value)}
            placeholder="內容只會送往 Tauri backend 並存入 OS vault"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={"credential-header-" + providerId}>Header</Label>
          <Input
            id={"credential-header-" + providerId}
            value={header}
            onChange={(event) => setHeader(event.target.value)}
            placeholder="authorization"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={"credential-prefix-" + providerId}>Prefix</Label>
          <Input
            id={"credential-prefix-" + providerId}
            value={prefix}
            onChange={(event) => setPrefix(event.target.value)}
            placeholder="Bearer "
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={"credential-priority-" + providerId}>優先序</Label>
          <Input
            id={"credential-priority-" + providerId}
            type="number"
            value={priority}
            onChange={(event) => setPriority(event.target.value)}
          />
        </div>
        <div className="flex items-end">
          <Button
            type="button"
            className="w-full"
            disabled={!data?.vaultAvailable || saveCredential.isPending}
            onClick={() => void addCredential()}
          >
            {saveCredential.isPending ? (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            ) : (
              <Plus className="mr-2 h-4 w-4" />
            )}
            新增 Credential
          </Button>
        </div>
      </div>
    </section>
  );
}

function CredentialRow({
  credential,
  busy,
  onToggle,
  onDelete,
}: {
  credential: ProviderCredential;
  busy: boolean;
  onToggle: (enabled: boolean) => void;
  onDelete: () => void;
}) {
  const limitingQuota = credential.quotas
    .filter((quota) => quota.remainingRatio !== null)
    .sort((left, right) => left.remainingRatio! - right.remainingRatio!)[0];

  return (
    <div className="flex items-center justify-between gap-3 rounded-md border border-border p-3">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">{credential.label}</span>
          <code className="text-xs text-muted-foreground">
            {credential.maskedHint}
          </code>
          <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] uppercase text-muted-foreground">
            {credential.status}
          </span>
        </div>
        <p className="mt-1 text-xs text-muted-foreground">
          {limitingQuota?.remainingRatio !== null &&
          limitingQuota?.remainingRatio !== undefined
            ? "剩餘 " + Math.round(limitingQuota.remainingRatio * 100) + "%"
            : "尚無 quota snapshot"}
          {limitingQuota?.resetAt
            ? " · reset " + new Date(limitingQuota.resetAt).toLocaleString()
            : ""}
          {credential.lastUsedAt
            ? " · 最近使用 " + new Date(credential.lastUsedAt).toLocaleString()
            : ""}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Switch
          checked={credential.enabled}
          onCheckedChange={onToggle}
          aria-label={"Toggle " + credential.label}
        />
        <Button
          type="button"
          size="icon"
          variant="ghost"
          disabled={busy}
          onClick={onDelete}
          aria-label={"Delete " + credential.label}
        >
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      </div>
    </div>
  );
}
