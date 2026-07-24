import { describe, expect, it } from "vitest";
import {
  remainingColor,
  remainingPercent,
} from "@/components/SubscriptionQuotaFooter";
import { resolveQuotaRefreshIntervalMs } from "@/lib/query/subscription";

describe("subscription remaining quota", () => {
  it("shows remaining percentage instead of utilization", () => {
    expect(remainingPercent(7)).toBe(93);
    expect(remainingPercent(100)).toBe(0);
    expect(remainingPercent(-5)).toBe(100);
    expect(remainingPercent(120)).toBe(0);
  });

  it("uses warning colors when remaining quota is low", () => {
    expect(remainingColor(9)).toContain("red");
    expect(remainingColor(25)).toContain("orange");
    expect(remainingColor(93)).toContain("green");
  });

  it("supports a five-second official quota refresh interval", () => {
    expect(resolveQuotaRefreshIntervalMs(true, 5_000)).toBe(5_000);
    expect(resolveQuotaRefreshIntervalMs(true, 0)).toBe(false);
    expect(resolveQuotaRefreshIntervalMs(false, 5_000)).toBe(false);
  });
});
