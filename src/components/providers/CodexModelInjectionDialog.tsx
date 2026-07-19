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
  fetchCodexOauthModels,
  fetchModelsForConfig,
  showFetchModelsError,
  type FetchedModel,
} from "@/lib/api/model-fetch";
import { codexModelRoutesApi } from "@/lib/api/codexModelRoutes";
import { useUpdateProviderMutation } from "@/lib/query/mutations";
import type { CodexCatalogModel, Provider } from "@/types";
import {
  buildCodexInjectedProvider,
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
  const updateProvider = useUpdateProviderMutation("codex");
  const connection = useMemo(
    () => codexProviderConnection(provider),
    [provider],
  );
  const configured = useMemo(() => configuredCodexModels(provider), [provider]);
  const [available, setAvailable] = useState<FetchedModel[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [manualModel, setManualModel] = useState("");
  const [fetching, setFetching] = useState(false);

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
        displayName: entry.ownedBy || entry.id,
      }));
    if (selectedModels.length === 0) {
      toast.error("請至少選擇一個第三方模型");
      return;
    }
    try {
      const updated = buildCodexInjectedProvider(provider, selectedModels);
      await updateProvider.mutateAsync({ provider: updated });
      const cachedOfficialModels =
        await codexModelRoutesApi.getCachedOfficialModels();
      const oauthModels =
        cachedOfficialModels.length > 0
          ? []
          : await fetchCodexOauthModels().catch(() => []);
      const officialCatalog =
        cachedOfficialModels.length > 0
          ? cachedOfficialModels
          : oauthModels.map((entry) => ({
              model: entry.id,
              displayName: entry.ownedBy || entry.id,
            }));
      if (officialCatalog.length === 0) {
        throw new Error(
          "找不到官方 Codex 模型清單，請先在 Codex App 使用一次官方模型後再試。",
        );
      }
      await codexModelRoutesApi.apply(
        provider.id,
        officialCatalog,
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
      toast.success("官方與第三方模型已共存於 Codex；請重新啟動 Codex App");
      onOpenChange(false);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  const rollback = async () => {
    try {
      await codexModelRoutesApi.rollback();
      await refreshProxyQueries();
      toast.success("已還原官方 Codex 設定；請重新啟動 Codex App");
      onOpenChange(false);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  const pending = fetching || updateProvider.isPending;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>Codex 模型共生 · {provider.name}</DialogTitle>
          <DialogDescription>
            將選定的第三方模型加入 Codex App，同時保留官方模型。選官方模型會走
            OpenAI，選第三方模型才會走此 Provider。
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
            還原官方設定
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
            套用共生模型清單
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
