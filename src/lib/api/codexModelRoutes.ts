import { invoke } from "@tauri-apps/api/core";

export interface CatalogModelInput {
  model: string;
  displayName?: string;
  contextWindow?: number;
  defaultReasoningLevel?: string;
  supportedReasoningLevels?: Array<{
    effort: string;
    description: string;
  }>;
}

export interface CodexModelRoute {
  alias: string;
  providerId: string;
  upstreamModel: string;
  displayName: string;
  contextWindow?: number;
}

export interface CodexModelRouteSettings {
  officialModels: CatalogModelInput[];
  routes: CodexModelRoute[];
}

export interface CodexDesktopRefreshResult {
  stoppedProcesses: number;
  respawned: boolean;
}

export const codexModelRoutesApi = {
  get(): Promise<CodexModelRouteSettings> {
    return invoke("get_codex_model_routes");
  },
  getCachedOfficialModels(): Promise<CatalogModelInput[]> {
    return invoke("get_codex_cached_official_models");
  },
  apply(
    providerId: string,
    officialModels: CatalogModelInput[],
    thirdPartyModels: CatalogModelInput[],
  ): Promise<CodexModelRouteSettings> {
    return invoke("apply_codex_model_routes", {
      providerId,
      officialModels,
      thirdPartyModels,
    });
  },
  refreshDesktopModelService(): Promise<CodexDesktopRefreshResult> {
    return invoke("refresh_codex_desktop_model_service");
  },
  rollback(): Promise<void> {
    return invoke("rollback_codex_model_routes");
  },
};
