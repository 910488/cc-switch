import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  UsageHeatmap,
  heatmapValueForView,
} from "@/components/usage/UsageHeatmap";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (_key: string, options?: { defaultValue?: string }) =>
      options?.defaultValue ?? _key,
    i18n: { resolvedLanguage: "zh-TW", language: "zh-TW" },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useUsageTrends: () => ({ data: [], isLoading: false }),
}));

describe("UsageHeatmap view aggregation", () => {
  it("selects the value for daily, weekly, and cumulative views", () => {
    expect(heatmapValueForView("daily", 10, 70, 170)).toBe(10);
    expect(heatmapValueForView("weekly", 10, 70, 170)).toBe(70);
    expect(heatmapValueForView("cumulative", 10, 70, 170)).toBe(170);
  });

  it("redraws the heatmap when the view button is clicked", () => {
    const { container } = render(
      <UsageHeatmap refreshIntervalMs={0} appType="codex" />,
    );

    expect(
      container.querySelector('[data-heatmap-view="daily"]'),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "每週" }));
    expect(
      container.querySelector('[data-heatmap-view="weekly"]'),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "每週" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    fireEvent.click(screen.getByRole("button", { name: "累計" }));
    expect(
      container.querySelector('[data-heatmap-view="cumulative"]'),
    ).toBeInTheDocument();
  });
});
