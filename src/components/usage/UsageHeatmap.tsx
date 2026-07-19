import { useMemo, useState } from "react";
import { Flame, Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { useUsageTrends } from "@/lib/query/usage";
import { cn } from "@/lib/utils";
import type { UsageRangeSelection, UsageScopeFilters } from "@/types/usage";
import { formatTokensShort, getResolvedLang } from "./format";

type HeatmapView = "daily" | "weekly" | "cumulative";

interface UsageHeatmapProps extends UsageScopeFilters {
  refreshIntervalMs: number;
}

const WEEKS = 53;

function localDateKey(date: Date): string {
  return [
    date.getFullYear(),
    String(date.getMonth() + 1).padStart(2, "0"),
    String(date.getDate()).padStart(2, "0"),
  ].join("-");
}

function levelFor(value: number, maximum: number): number {
  if (value <= 0 || maximum <= 0) return 0;
  const fraction = value / maximum;
  if (fraction > 0.75) return 4;
  if (fraction > 0.5) return 3;
  if (fraction > 0.25) return 2;
  return 1;
}

const LEVEL_CLASS = [
  "bg-muted/60",
  "bg-emerald-500/25",
  "bg-emerald-500/45",
  "bg-emerald-500/70",
  "bg-emerald-500",
] as const;

export function UsageHeatmap({
  appType,
  providerName,
  model,
  refreshIntervalMs,
}: UsageHeatmapProps) {
  const { t, i18n } = useTranslation();
  const [view, setView] = useState<HeatmapView>("daily");
  const language = getResolvedLang(i18n);
  const locale = language.toLowerCase().startsWith("zh") ? "zh-TW" : language;

  const { range, today, start, weeks } = useMemo(() => {
    const todayValue = new Date();
    todayValue.setHours(0, 0, 0, 0);
    const startValue = new Date(todayValue);
    startValue.setDate(startValue.getDate() - startValue.getDay() - 52 * 7);
    const columns = Array.from({ length: WEEKS }, (_, weekIndex) =>
      Array.from({ length: 7 }, (_, dayIndex) => {
        const date = new Date(startValue);
        date.setDate(startValue.getDate() + weekIndex * 7 + dayIndex);
        return date;
      }),
    );
    const selection: UsageRangeSelection = {
      preset: "custom",
      customStartDate: Math.floor(startValue.getTime() / 1000),
      liveEndTime: true,
    };
    return {
      range: selection,
      today: todayValue,
      start: startValue,
      weeks: columns,
    };
  }, []);

  const { data: trends = [], isLoading } = useUsageTrends(
    range,
    { appType, providerName, model },
    { refetchInterval: refreshIntervalMs > 0 ? refreshIntervalMs : false },
  );

  const metrics = useMemo(() => {
    const daily = new Map<string, number>();
    for (const row of trends) {
      daily.set(row.date.slice(0, 10), row.totalTokens || 0);
    }
    const weekTotals = weeks.map((column) =>
      column.reduce(
        (sum, date) => sum + (daily.get(localDateKey(date)) ?? 0),
        0,
      ),
    );
    const cumulative: number[] = [];
    let running = 0;
    for (const total of weekTotals) {
      running += total;
      cumulative.push(running);
    }
    const dailyValues = [...daily.values()];
    const maxDaily = Math.max(0, ...dailyValues);
    const maxWeekly = Math.max(0, ...weekTotals);
    let streak = 0;
    const cursor = new Date(today);
    while ((daily.get(localDateKey(cursor)) ?? 0) > 0) {
      streak += 1;
      cursor.setDate(cursor.getDate() - 1);
    }
    return {
      daily,
      weekTotals,
      cumulative,
      total: running,
      maxDaily,
      maxWeekly,
      activeDays: dailyValues.filter((value) => value > 0).length,
      streak,
    };
  }, [today, trends, weeks]);

  const monthMarks = useMemo(() => {
    const marks: Array<{ week: number; label: string }> = [];
    let previousMonth = -1;
    for (let week = 0; week < weeks.length; week += 1) {
      const month = weeks[week][0].getMonth();
      if (month !== previousMonth) {
        if (!marks.length || week - marks[marks.length - 1].week >= 3) {
          marks.push({
            week,
            label: weeks[week][0].toLocaleDateString(locale, {
              month: "short",
            }),
          });
        }
        previousMonth = month;
      }
    }
    return marks;
  }, [locale, weeks]);

  if (isLoading) {
    return (
      <div className="flex h-48 items-center justify-center rounded-xl border border-border/50 bg-card/40">
        <Loader2 className="h-7 w-7 animate-spin text-muted-foreground/40" />
      </div>
    );
  }

  return (
    <section className="rounded-xl border border-border/50 bg-card/40 p-5 backdrop-blur-sm">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h3 className="flex items-center gap-2 text-lg font-semibold">
            <Flame className="h-5 w-5 text-emerald-500" />
            {t("usage.heatmap", { defaultValue: "使用量熱圖" })}
          </h3>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("usage.heatmapDescription", {
              defaultValue: "最近 53 週的 Token 使用紀錄",
            })}
          </p>
        </div>
        <div className="flex rounded-lg border border-border/60 bg-muted/30 p-1">
          {(["daily", "weekly", "cumulative"] as const).map((mode) => (
            <Button
              key={mode}
              type="button"
              size="sm"
              variant={view === mode ? "secondary" : "ghost"}
              className="h-7 px-2.5 text-xs"
              onClick={() => setView(mode)}
            >
              {t(`usage.heatmapView.${mode}`, {
                defaultValue:
                  mode === "daily"
                    ? "每日"
                    : mode === "weekly"
                      ? "每週"
                      : "累計",
              })}
            </Button>
          ))}
        </div>
      </div>

      <div className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-4">
        {[
          [formatTokensShort(metrics.total, language), "累計 Token"],
          [formatTokensShort(metrics.maxDaily, language), "單日峰值"],
          [metrics.activeDays.toLocaleString(locale), "有紀錄天數"],
          [`${metrics.streak} 天`, "目前連續紀錄"],
        ].map(([value, label]) => (
          <div
            key={label}
            className="rounded-lg border border-border/50 bg-background/50 p-3"
          >
            <div className="text-base font-semibold text-foreground">
              {value}
            </div>
            <div className="mt-0.5 text-[11px] text-muted-foreground">
              {label}
            </div>
          </div>
        ))}
      </div>

      <div className="mt-5 overflow-x-auto pb-2">
        <div className="min-w-[760px]">
          <div className="relative mb-1 h-5 text-[10px] text-muted-foreground">
            {monthMarks.map((mark) => (
              <span
                key={`${mark.week}:${mark.label}`}
                className="absolute"
                style={{ left: `${(mark.week / WEEKS) * 100}%` }}
              >
                {mark.label}
              </span>
            ))}
          </div>
          <div
            className="grid gap-1"
            style={{
              gridAutoFlow: "column",
              gridTemplateColumns: `repeat(${WEEKS}, minmax(0, 1fr))`,
              gridTemplateRows: "repeat(7, 12px)",
            }}
          >
            {weeks.flatMap((column, weekIndex) =>
              column.map((date) => {
                const future = date.getTime() > today.getTime();
                const dailyValue = metrics.daily.get(localDateKey(date)) ?? 0;
                const value =
                  view === "daily"
                    ? dailyValue
                    : view === "weekly"
                      ? metrics.weekTotals[weekIndex]
                      : metrics.cumulative[weekIndex];
                const maximum =
                  view === "daily"
                    ? metrics.maxDaily
                    : view === "weekly"
                      ? metrics.maxWeekly
                      : metrics.total;
                const periodLabel =
                  view === "daily"
                    ? date.toLocaleDateString(locale)
                    : view === "weekly"
                      ? `${column[0].toLocaleDateString(locale)} 當週`
                      : `截至 ${column[0].toLocaleDateString(locale)} 當週`;
                return (
                  <div
                    key={localDateKey(date)}
                    title={
                      future
                        ? undefined
                        : `${periodLabel}：${value.toLocaleString(locale)} Token`
                    }
                    className={cn(
                      "h-3 rounded-[3px] ring-1 ring-inset ring-border/20 transition-transform hover:scale-125",
                      future
                        ? "bg-transparent ring-transparent"
                        : LEVEL_CLASS[levelFor(value, maximum)],
                    )}
                  />
                );
              }),
            )}
          </div>
          <div className="mt-2 flex items-center justify-end gap-1 text-[10px] text-muted-foreground">
            <span>少</span>
            {LEVEL_CLASS.map((className, index) => (
              <span
                key={index}
                className={cn(
                  "h-3 w-3 rounded-[3px] ring-1 ring-inset ring-border/20",
                  className,
                )}
              />
            ))}
            <span>多</span>
          </div>
        </div>
      </div>
      <p className="mt-1 text-[11px] text-muted-foreground">
        {t("usage.heatmapRange", {
          defaultValue: "資料起點",
        })}
        ：{start.toLocaleDateString(locale)}
      </p>
    </section>
  );
}
