import { invoke } from "@tauri-apps/api/core";
import type { AutoReviewSettings, AutoReviewStats } from "@/types/autoReview";

export const autoReviewApi = {
  getSettings(): Promise<AutoReviewSettings> {
    return invoke("get_auto_review_settings");
  },

  updateSettings(settings: AutoReviewSettings): Promise<AutoReviewSettings> {
    return invoke("update_auto_review_settings", { settings });
  },

  getStats(): Promise<AutoReviewStats> {
    return invoke("get_auto_review_stats");
  },
};
