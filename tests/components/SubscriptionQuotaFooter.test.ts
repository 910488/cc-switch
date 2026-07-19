import { describe, expect, it } from "vitest";
import {
  remainingColor,
  remainingPercent,
} from "@/components/SubscriptionQuotaFooter";

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
});
