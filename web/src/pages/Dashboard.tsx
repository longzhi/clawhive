import { useEffect } from "react";
import { useNavigate } from "react-router-dom";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Activity, Users, MessageSquare, Server, Radio } from "lucide-react";
import { useMetrics, useSetupStatus } from "@/hooks/use-api";
import { EventStream } from "@/components/dashboard/event-stream";

export default function Dashboard() {
  const { data: metrics } = useMetrics();
  const { data: setupStatus } = useSetupStatus();
  const navigate = useNavigate();

  useEffect(() => {
    if (setupStatus?.needs_setup) {
      navigate("/setup", { replace: true });
    }
  }, [setupStatus, navigate]);

  return (
    <div className="grid gap-4 md:gap-8">
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
