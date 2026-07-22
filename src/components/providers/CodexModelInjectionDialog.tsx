import { useEffect, useMemo, useState } from "react";
import { Download, Loader2, Plus, RotateCcw, Save } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  fetchModelsForConfig,
  showFetchModelsError,
  type FetchedModel,
} from "@/lib/api/model-fetch";
import { codexModelRoutesApi } from "@/lib/api/codexModelRoutes";
import type { CodexCatalogModel, Provider } from "@/types";
import {
  codexProviderConnection,
  configuredCodexModels,
} from "./codexModelInjection";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  provider: Provider;
}

export function CodexModelInjectionDialog({
  open,
  onOpenChange,
  provider,
}: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const connection = useMemo(
    () => codexProviderConnection(provider),
    [provider],
  );
  const configured = useMemo(() => configuredCodexModels(provider), [provider]);
  const [available, setAvailable] = useState<FetchedModel[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [manualModel, setManualModel] = useState("");
  const [fetching, setFetching] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!open) return;
    setAvailable(
      configured.map((entry) => ({
        id: entry.model,
        ownedBy: entry.displayName || null,
      })),
    );
    setSelected(new Set(configured.map((entry) => entry.model)));
    setManualModel("");
  }, [configured, open]);

  const fetchModels = async () => {
    if (!connection.baseUrl || !connection.apiKey) {
      showFetchModelsError(null, t, {
        hasApiKey: Boolean(connection.apiKey),
        hasBaseUrl: Boolean(connection.baseUrl),
      });
      return;
    }
    setFetching(true);
    try {
      const models = await fetchModelsForConfig(
        connection.baseUrl,
        connection.apiKey,
        connection.configuredAsFullUrl,
        undefined,
        provider.meta?.customUserAgent,
      );
      setAvailable(models);
      setSelected((current) => {
        const valid = new Set(models.map((entry) => entry.id));
        const next = new Set([...current].filter((id) => valid.has(id)));
        if (next.size === 0 && models.length > 0) next.add(models[0].id);
        return next;
      });
      toast.success(`已取得 ${models.length} 個模型`);
    } catch (error) {
      showFetchModelsError(error, t);
    } finally {
      setFetching(false);
    }
  };

  const addManualModel = () => {
    const id = manualModel.trim();
    if (!id) return;
    setAvailable((current) =>
      current.some((entry) => entry.id === id)
        ? current
        : [...current, { id, ownedBy: null }],
    );
    setSelected((current) => new Set(current).add(id));
    setManualModel("");
  };

  const refreshProxyQueries = async () => {
    await queryClient.invalidateQueries({ queryKey: ["providers", "codex"] });
    await queryClient.invalidateQueries({ queryKey: ["proxyTakeoverStatus"] });
    await queryClient.invalidateQueries({ queryKey: ["proxyStatus"] });
  };

  const apply = async () => {
    const selectedModels: CodexCatalogModel[] = available
      .filter((entry) => selected.has(entry.id))
      .map((entry) => ({
        model: entry.id,
        // `owned_by` identifies the API vendor (often just "openai"), not the
        // model's user-facing name. Keep the exact upstream model ID visible.
        displayName: entry.id,
      }));
    if (selectedModels.length === 0) {
      toast.error("請至少選擇一個第三方模型");
      return;
    }
    setSaving(true);
    try {
      const cachedOfficialModels =
        await codexModelRoutesApi.getCachedOfficialModels();
      if (cachedOfficialModels.length === 0) {
        throw new Error(
          "找不到 Codex Desktop 的官方模型快取。請先關閉 Proxy、啟動 Codex 並確認原生模型清單載入完成後再試。",
        );
      }
      await codexModelRoutesApi.apply(
        provider.id,
        cachedOfficialModels,
        selectedModels.map((entry) => ({
          model: entry.model,
          displayName: entry.displayName,
          contextWindow:
            typeof entry.contextWindow === "number"
              ? entry.contextWindow
              : undefined,
        })),
      );
      await refreshProxyQueries();
      onOpenChange(false);
      toast.success("Codex 模型設定已儲存；Proxy 開啟時會自動套用。");
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setSaving(false);
    }
  };

  const rollback = async () => {
    setSaving(true);
    try {
      await codexModelRoutesApi.rollback();
      await refreshProxyQueries();
      onOpenChange(false);
      toast.success("Codex 第三方模型設定已清除。");
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setSaving(false);
    }
  };

  const pending = fetching || saving;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>設定 Codex 模型 · {provider.name}</DialogTitle>
          <DialogDescription>
            設定要在 Codex App 顯示的第三方模型，並保留官方模型。Codex Proxy
            開啟時會自動套用此設定：官方模型走 OpenAI，第三方模型走此 Provider。
            若 Codex App 已開啟，請正常關閉後再開啟以載入更新後的模型清單。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-6 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-border bg-muted/30 p-3">
            <div className="min-w-0 text-xs">
              <p className="font-medium text-foreground">
                {connection.baseUrl || "尚未設定 API URL"}
              </p>
              <p className="mt-1 text-muted-foreground">
                {configured.length > 0
                  ? `目前已設定 ${configured.length} 個模型`
                  : "尚未取得或加入第三方模型"}
              </p>
            </div>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => void fetchModels()}
              disabled={pending}
            >
              {fetching ? (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              ) : (
                <Download className="mr-2 h-4 w-4" />
              )}
              從 API 取得模型
            </Button>
          </div>

          <div className="flex gap-2">
            <Input
              value={manualModel}
              onChange={(event) => setManualModel(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  addManualModel();
                }
              }}
              placeholder="手動輸入模型 ID，例如 GLM-5.2p"
            />
            <Button
              type="button"
              variant="outline"
              onClick={addManualModel}
              disabled={!manualModel.trim()}
            >
              <Plus className="mr-1.5 h-4 w-4" />
              加入
            </Button>
          </div>

          <div className="max-h-72 space-y-1 overflow-y-auto rounded-md border border-border p-2">
            {available.map((model) => (
              <Label
                key={model.id}
                className="flex cursor-pointer items-center gap-3 rounded px-2 py-2 hover:bg-muted/60"
              >
                <Checkbox
                  checked={selected.has(model.id)}
                  onCheckedChange={(checked) =>
                    setSelected((current) => {
                      const next = new Set(current);
                      if (checked) next.add(model.id);
                      else next.delete(model.id);
                      return next;
                    })
                  }
                />
                <span className="min-w-0 flex-1 truncate font-mono text-xs">
                  {model.id}
                </span>
                {model.ownedBy && (
                  <span className="text-[11px] text-muted-foreground">
                    {model.ownedBy}
                  </span>
                )}
              </Label>
            ))}
            {available.length === 0 && (
              <p className="p-4 text-center text-xs text-muted-foreground">
                請從 API 取得模型，或手動加入模型 ID。
              </p>
            )}
          </div>
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => void rollback()}
            disabled={pending}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            清除第三方模型設定
          </Button>
          <Button
            type="button"
            onClick={() => void apply()}
            disabled={pending || selected.size === 0}
          >
            {pending ? (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            ) : (
              <Save className="mr-2 h-4 w-4" />
            )}
            儲存 Codex 模型設定
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
