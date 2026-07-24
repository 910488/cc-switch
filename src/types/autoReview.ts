export type AutoReviewMode = "off" | "auto" | "always";

export interface AutoReviewSettings {
  mode: AutoReviewMode;
  fallbackProviderId: string;
  fallbackModel: string;
  fallbackEffort: string;
}

export interface AutoReviewStats {
  fallbacks: number;
  officialAttempts: number;
  officialQuota429: number;
  successfulFallbacks: number;
  failedFallbacks: number;
  lastFallbackAt: string | null;
  lastFallbackProviderId: string | null;
  lastFallbackModel: string | null;
  lastError: string | null;
}
