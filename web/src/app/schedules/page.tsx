"use client";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useRunSchedule, useSchedules, useToggleSchedule } from "@/hooks/use-api";
import { formatDistanceToNow } from "date-fns";
import { AlertTriangle, Clock3, Loader2, Play } from "lucide-react";
import { toast } from "sonner";

function formatSchedule(schedule: {
  kind: "cron" | "at" | "every";
  expr?: string;
  tz?: string;
  at?: string;
  interval_ms?: number;
}) {
  switch (schedule.kind) {
    case "cron":
      return `${schedule.expr ?? "-"} @ ${schedule.tz ?? "UTC"}`;
    case "at":
      return schedule.at ?? "-";
    case "every":
      return `${schedule.interval_ms ?? 0}ms interval`;
    default:
      return "-";
  }
}

function statusVariant(status: "ok" | "error" | "skipped" | null) {
  if (status === "ok") return "text-green-700 border-green-200 bg-green-50";
  if (status === "error") return "text-red-700 border-red-200 bg-red-50";
  if (status === "skipped") return "text-slate-700 border-slate-200 bg-slate-50";
  return "";
}

export default function SchedulesPage() {
  const { data: schedules, isLoading } = useSchedules();
  const runMutation = useRunSchedule();
  const toggleMutation = useToggleSchedule();

  if (isLoading) {
    return (
      <div className="flex justify-center p-8">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    );
  }

  return (
    <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
      {(schedules ?? []).map((item) => {
        const nextRunText = item.next_run_at
          ? formatDistanceToNow(new Date(item.next_run_at), { addSuffix: true })
          : "-";

        return (
          <Card key={item.schedule_id}>
            <CardHeader className="space-y-2">
              <div className="flex items-start justify-between gap-3">
                <div>
                  <CardTitle className="text-base">{item.name}</CardTitle>
                  <CardDescription className="mt-1 text-xs">{item.schedule_id}</CardDescription>
                </div>
                <Switch
                  checked={item.enabled}
                  disabled={toggleMutation.isPending}
                  onCheckedChange={async (enabled) => {
                    try {
                      await toggleMutation.mutateAsync({ id: item.schedule_id, enabled });
                      toast.success(`${enabled ? "Enabled" : "Disabled"}: ${item.name}`);
                    } catch {
                      toast.error(`Failed to update ${item.name}`);
                    }
                  }}
                />
              </div>
              {item.description && <CardDescription>{item.description}</CardDescription>}
            </CardHeader>
            <CardContent className="space-y-3 text-sm">
              <div className="flex items-center gap-2 text-muted-foreground">
                <Clock3 className="h-4 w-4" />
                <span className="truncate">{formatSchedule(item.schedule)}</span>
              </div>

              <div className="flex items-center justify-between">
                <span className="text-muted-foreground">Next run</span>
                <span className="font-medium">{nextRunText}</span>
              </div>

              <div className="flex items-center justify-between">
                <span className="text-muted-foreground">Last status</span>
                <Badge variant="outline" className={statusVariant(item.last_run_status)}>
                  {item.last_run_status ?? "unknown"}
                </Badge>
              </div>

              {item.consecutive_errors > 0 && (
                <div className="flex items-center gap-2 text-amber-700 text-xs bg-amber-50 border border-amber-200 rounded-md px-2 py-1.5">
                  <AlertTriangle className="h-3.5 w-3.5" />
                  Consecutive errors: {item.consecutive_errors}
                </div>
              )}

              <Button
                className="w-full"
                variant="secondary"
                disabled={runMutation.isPending}
                onClick={async () => {
                  try {
                    await runMutation.mutateAsync(item.schedule_id);
                    toast.success(`Triggered: ${item.name}`);
                  } catch {
                    toast.error(`Failed to run ${item.name}`);
                  }
                }}
              >
                <Play className="h-4 w-4 mr-2" />
                Run now
              </Button>
            </CardContent>
          </Card>
        );
      })}
    </div>
  );
}
