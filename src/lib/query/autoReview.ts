import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { autoReviewApi } from "@/lib/api/autoReview";
import type { AutoReviewSettings } from "@/types/autoReview";

export function useAutoReviewSettings() {
  return useQuery({
    queryKey: ["autoReviewSettings"],
    queryFn: autoReviewApi.getSettings,
  });
}

export function useUpdateAutoReviewSettings() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (settings: AutoReviewSettings) =>
      autoReviewApi.updateSettings(settings),
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["autoReviewSettings"] }),
  });
}

export function useAutoReviewStats(polling = true) {
  return useQuery({
    queryKey: ["autoReviewStats"],
    queryFn: autoReviewApi.getStats,
    refetchInterval: polling ? 5000 : false,
  });
}
