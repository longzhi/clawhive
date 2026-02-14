"use client";

import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { MessageCircle, Loader2, Key } from "lucide-react";
import { useChannels, useUpdateChannels } from "@/hooks/use-api";
import { toast } from "sonner";

const CHANNEL_META: Record<string, { label: string; description: string; color: string }> = {
  telegram: { label: "Telegram", description: "Bot API integration", color: "text-blue-500" },
  discord: { label: "Discord", description: "Gateway connection", color: "text-indigo-500" },
};

export default function ChannelsPage() {
  const { data: channels, isLoading } = useChannels();
  const updateChannels = useUpdateChannels();
  const [tokens, setTokens] = useState<Record<string, string>>({});

  const handleToggle = async (channelKey: string, enabled: boolean) => {
    if (!channels) return;
    const updated = JSON.parse(JSON.stringify(channels));
    if (updated[channelKey]) {
      updated[channelKey].enabled = enabled;
    }
    try {
      await updateChannels.mutateAsync(updated);
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
      toast.success("Token saved");
      setTokens(prev => ({ ...prev, [tokenKey]: "" }));
    } catch {
      toast.error("Failed to save token");
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
    <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
      {channelKeys.map((key) => {
        const meta = CHANNEL_META[key];
        const channel = channels?.[key];
        const enabled = channel?.enabled ?? false;
        const connectors = channel?.connectors ?? [];

        return (
          <Card key={key}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <div className="flex flex-col space-y-1">
                <CardTitle>{meta.label}</CardTitle>
                <CardDescription>{meta.description}</CardDescription>
              </div>
              <div className="flex items-center gap-4">
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
                connectors.map((connector: any, idx: number) => {
                  const tokenKey = `${key}-${idx}`;
                  const isEnvRef = connector.token?.startsWith("${");
                  return (
                    <div key={connector.connector_id} className="flex flex-col gap-2 border-b pb-3 last:border-0">
                      <div className="flex items-center justify-between">
                        <span className="text-sm font-medium">{connector.connector_id}</span>
                        <Badge variant={enabled ? "outline" : "secondary"} className={enabled ? "text-green-600 border-green-200 bg-green-50" : ""}>
                          {enabled ? "Active" : "Inactive"}
                        </Badge>
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
  );
}
