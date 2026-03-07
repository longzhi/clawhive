import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Activity, Users, MessageSquare, Server, Radio } from "lucide-react";
import { useMetrics, useSetupStatus, useSessions, useAgents } from "@/hooks/use-api";
import { EventStream } from "@/components/dashboard/event-stream";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";

function UpdatedAgo({ dataUpdatedAt }: { dataUpdatedAt: number }) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick(t => t + 1), 5000);
    return () => clearInterval(id);
  }, []);
  
  if (!dataUpdatedAt) return null;
  const seconds = Math.floor((Date.now() - dataUpdatedAt) / 1000);
  const text = seconds < 5 ? "just now" : seconds < 60 ? `${seconds}s ago` : `${Math.floor(seconds / 60)}m ago`;
  return <span className="text-xs text-muted-foreground">Updated {text}</span>;
}

// ---------------------------------------------------------------------------
// Dashboard Skeleton
// ---------------------------------------------------------------------------
function DashboardSkeleton() {
  return (
    <div className="grid gap-4 md:gap-8">
      <div className="grid gap-4 grid-cols-2 md:grid-cols-5">
        {Array.from({ length: 5 }).map((_, i) => (
          <Card key={i}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <Skeleton className="h-4 w-24" />
              <Skeleton className="h-4 w-4" />
            </CardHeader>
            <CardContent>
              <Skeleton className="h-8 w-12" />
              <Skeleton className="h-3 w-16 mt-1" />
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}

export default function Dashboard() {
  const { data: metrics, dataUpdatedAt: metricsUpdatedAt, isLoading: isLoadingMetrics, isError: isErrorMetrics, error: errorMetrics, refetch: refetchMetrics } = useMetrics();
  const { data: setupStatus } = useSetupStatus();
  const { data: sessions } = useSessions();
  const { data: agents } = useAgents();

  const navigate = useNavigate();

  useEffect(() => {
    if (setupStatus?.needs_setup) {
      navigate("/setup", { replace: true });
    }
  }, [setupStatus, navigate]);

  if (isLoadingMetrics) return <DashboardSkeleton />;
  if (isErrorMetrics) return <ErrorState message={errorMetrics?.message} onRetry={refetchMetrics} />

  return (
    <div className="grid gap-4 md:gap-8">
      <div className="flex items-center justify-between mb-4">
        <h1 className="text-lg font-semibold">Dashboard</h1>
        <UpdatedAgo dataUpdatedAt={metricsUpdatedAt} />
      </div>
      <div className="grid gap-4 grid-cols-2 md:grid-cols-5">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">Total Sessions</CardTitle>
            <MessageSquare className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{metrics?.sessions_total ?? "-"}</div>
            <p className="text-xs text-muted-foreground">Recorded</p>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">Active Agents</CardTitle>
            <Users className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{metrics?.agents_active ?? "-"}</div>
            <p className="text-xs text-muted-foreground">Online now</p>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">Total Agents</CardTitle>
            <Activity className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{metrics?.agents_total ?? "-"}</div>
            <p className="text-xs text-muted-foreground">Configured</p>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">Providers</CardTitle>
            <Server className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{metrics?.providers_total ?? "-"}</div>
            <p className="text-xs text-muted-foreground">Configured</p>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">Channels</CardTitle>
            <Radio className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{metrics?.channels_total ?? "-"}</div>
            <p className="text-xs text-muted-foreground">Configured</p>
          </CardContent>
        </Card>
      </div>

      <div className="col-span-full">
        <EventStream />
      </div>
    </div>
  );
}
