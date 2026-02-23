"use client";

import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { MessageCircle, Loader2, Key, Trash2 } from "lucide-react";
import { type ConnectorConfig, useChannelStatus, useChannels, useRemoveConnector, useUpdateChannels } from "@/hooks/use-api";
import { toast } from "sonner";
import { AddConnectorDialog } from "@/components/channels/add-connector-dialog";
import { RestartBanner } from "@/components/restart-banner";

const CHANNEL_META: Record<string, { label: string; description: string; color: string }> = {
  telegram: { label: "Telegram", description: "Bot API integration", color: "text-blue-500" },
  discord: { label: "Discord", description: "Gateway connection", color: "text-indigo-500" },
};

export default function ChannelsPage() {
  const { data: channels, isLoading } = useChannels();
  const { data: statuses } = useChannelStatus();
  const updateChannels = useUpdateChannels();
  const removeConnector = useRemoveConnector();
  const [tokens, setTokens] = useState<Record<string, string>>({});
  const [restartRequired, setRestartRequired] = useState(false);

  const statusMap = new Map((statuses ?? []).map((item) => [`${item.kind}:${item.connector_id}`, item.status]));

  const handleToggle = async (channelKey: string, enabled: boolean) => {
    if (!channels) return;
    const updated = JSON.parse(JSON.stringify(channels));
    if (updated[channelKey]) {
      updated[channelKey].enabled = enabled;
    }
    try {
      await updateChannels.mutateAsync(updated);
      setRestartRequired(true);
      toast.success(`${CHANNEL_META[channelKey]?.label ?? channelKey} ${enabled ? "enabled" : "disabled"}`);
    } catch {
      toast.error(`Failed to update ${channelKey}`);
    }
  };

  const handleSaveToken = async (channelKey: string, connectorIdx: number) => {
    if (!channels) return;
    const tokenKey = `${channelKey}-${connectorIdx}`;
    const token = tokens[tokenKey];
    if (!token) return;

    const updated = JSON.parse(JSON.stringify(channels));
    if (updated[channelKey]?.connectors?.[connectorIdx]) {
      updated[channelKey].connectors[connectorIdx].token = token;
    }
    try {
      await updateChannels.mutateAsync(updated);
      setRestartRequired(true);
      toast.success("Token saved");
      setTokens(prev => ({ ...prev, [tokenKey]: "" }));
    } catch {
      toast.error("Failed to save token");
    }
  };

  const handleRemoveConnector = async (channelKey: string, connectorId: string) => {
    try {
      await removeConnector.mutateAsync({ kind: channelKey, connectorId });
      setRestartRequired(true);
      toast.success("Connector removed");
    } catch {
      toast.error("Failed to remove connector");
    }
  };

  if (isLoading) {
    return (
      <div className="flex justify-center p-8">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    );
  }

  const channelKeys = Object.keys(CHANNEL_META);

  return (
    <div>
      <RestartBanner visible={restartRequired} />
      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {channelKeys.map((key) => {
        const meta = CHANNEL_META[key];
        const channel = channels?.[key];
        const enabled = channel?.enabled ?? false;
        const connectors: ConnectorConfig[] = channel?.connectors ?? [];

        return (
          <Card key={key}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <div className="flex flex-col space-y-1">
                <CardTitle>{meta.label}</CardTitle>
                <CardDescription>{meta.description}</CardDescription>
              </div>
              <div className="flex items-center gap-4">
                <AddConnectorDialog kind={key} label={meta.label} onAdded={() => setRestartRequired(true)} />
                <Switch
                  checked={enabled}
                  onCheckedChange={(checked) => handleToggle(key, checked)}
                  disabled={updateChannels.isPending}
                />
                <MessageCircle className={`h-6 w-6 ${meta.color}`} />
              </div>
            </CardHeader>
            <CardContent className="grid gap-4 pt-4">
              {connectors.length > 0 ? (
                connectors.map((connector, idx: number) => {
                  const tokenKey = `${key}-${idx}`;
                  const isEnvRef = connector.token?.startsWith("${");
                  const runtimeStatus = statusMap.get(`${key}:${connector.connector_id}`) ?? "inactive";
                  const statusBadgeClass =
                    runtimeStatus === "connected"
                      ? "text-green-600 border-green-200 bg-green-50"
                      : runtimeStatus === "error"
                        ? "text-red-600 border-red-200 bg-red-50"
                        : "text-slate-600 border-slate-200 bg-slate-50";
                  return (
                    <div key={connector.connector_id} className="flex flex-col gap-2 border-b pb-3 last:border-0">
                      <div className="flex items-center justify-between">
                        <span className="text-sm font-medium">{connector.connector_id}</span>
                        <div className="flex items-center gap-2">
                          <Badge variant="outline" className={statusBadgeClass}>
                            {runtimeStatus === "connected" ? "Connected" : runtimeStatus === "error" ? "Error" : "Inactive"}
                          </Badge>
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7"
                            onClick={() => handleRemoveConnector(key, connector.connector_id)}
                            disabled={removeConnector.isPending}
                          >
                            <Trash2 className="h-4 w-4" />
                            <span className="sr-only">Delete connector</span>
                          </Button>
                        </div>
                      </div>
                      <div className="flex flex-col gap-1">
                        <div className="flex items-center gap-2">
                          <div className="relative flex-1">
                            <Key className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                            <Input
                              type="password"
                              placeholder="Enter bot token..."
                              className="pl-9 h-9 text-sm"
                              value={tokens[tokenKey] || ""}
                              onChange={(e) => setTokens(prev => ({ ...prev, [tokenKey]: e.target.value }))}
                            />
                          </div>
                          <Button
                            size="sm"
                            className="h-9"
                            onClick={() => handleSaveToken(key, idx)}
                            disabled={updateChannels.isPending || !tokens[tokenKey]}
                          >
                            Save
                          </Button>
                        </div>
                        <Badge variant={isEnvRef ? "secondary" : "outline"} className={`w-fit text-[10px] ${!isEnvRef ? "text-green-600 border-green-200 bg-green-50" : ""}`}>
                          {isEnvRef ? "Token not set" : "Token configured"}
                        </Badge>
                      </div>
                    </div>
                  );
                })
              ) : (
                <div className="text-sm text-muted-foreground">No connectors configured</div>
              )}
            </CardContent>
          </Card>
        );
        })}
      </div>
    </div>
  );
}
