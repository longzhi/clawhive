import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { MessageCircle, Loader2, Key, Trash2, Plus, ExternalLink } from "lucide-react";
import { type ConnectorConfig, useChannelStatus, useChannels, useRemoveConnector, useUpdateChannels, useAddConnector } from "@/hooks/use-api";
import { toast } from "sonner";
import { AddConnectorDialog } from "@/components/channels/add-connector-dialog";
import { RestartBanner } from "@/components/restart-banner";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";

const CHANNEL_META: Record<string, { label: string; description: string; color: string; tokenLink: string }> = {
  telegram: { label: "Telegram", description: "Bot API integration", color: "text-blue-500", tokenLink: "https://t.me/BotFather" },
  discord: { label: "Discord", description: "Gateway connection", color: "text-indigo-500", tokenLink: "https://discord.com/developers/applications" },
  slack: { label: "Slack", description: "Workspace bot", color: "text-purple-500", tokenLink: "https://api.slack.com/apps" },
  whatsapp: { label: "WhatsApp", description: "Business API", color: "text-green-500", tokenLink: "https://developers.facebook.com/apps/" },
  imessage: { label: "iMessage", description: "Apple Messages", color: "text-sky-500", tokenLink: "" },
};

// ---------------------------------------------------------------------------
// Add Channel Dialog — creates the channel kind + first connector in one go
// ---------------------------------------------------------------------------
function AddChannelDialog({
  existingKinds,
  onDone,
}: {
  existingKinds: Set<string>;
  onDone: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [selectedKind, setSelectedKind] = useState<string | null>(null);
  const [connectorId, setConnectorId] = useState("");
  const [token, setToken] = useState("");
  const updateChannels = useUpdateChannels();
  const addConnector = useAddConnector();
  const { data: channels } = useChannels();
  const [submitting, setSubmitting] = useState(false);

  const reset = () => {
    setSelectedKind(null);
    setConnectorId("");
    setToken("");
  };

  const handleSubmit = async () => {
    if (!selectedKind || !connectorId || !token) return;
    setSubmitting(true);
    try {
      // Step 1: Ensure the channel kind exists via PUT /api/channels
      const current = channels ?? {};
      if (!current[selectedKind]) {
        const merged = { ...current, [selectedKind]: { enabled: true, connectors: [] } };
        await updateChannels.mutateAsync(merged);
      }
      // Step 2: Add the connector
      await addConnector.mutateAsync({ kind: selectedKind, connectorId, token });
      toast.success(`${CHANNEL_META[selectedKind]?.label ?? selectedKind} channel added`);
      onDone();
      reset();
      setOpen(false);
    } catch {
      toast.error("Failed to add channel");
    } finally {
      setSubmitting(false);
    }
  };

  const meta = selectedKind ? CHANNEL_META[selectedKind] : null;

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); if (!v) reset(); }}>
      <DialogTrigger asChild>
        <Button size="sm" className="gap-1.5">
          <Plus className="h-4 w-4" />
          Add Channel
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Add Channel</DialogTitle>
          <DialogDescription>Connect a new messaging platform.</DialogDescription>
        </DialogHeader>

        <div className="grid grid-cols-3 gap-3">
          {Object.entries(CHANNEL_META).map(([kind, m]) => {
            const exists = existingKinds.has(kind);
            return (
              <button
                key={kind}
                onClick={() => !exists && setSelectedKind(kind)}
                disabled={exists}
                className={`rounded-lg border px-4 py-4 text-left transition-all ${
                  selectedKind === kind
                    ? "border-primary bg-primary/5 ring-1 ring-primary/20"
                    : exists
                      ? "border-border opacity-40 cursor-not-allowed"
                      : "border-border hover:border-primary/40 hover:bg-muted/50 cursor-pointer"
                }`}
              >
                <div className="text-sm font-medium">{m.label}</div>
                <div className="mt-0.5 text-xs text-muted-foreground">{m.description}</div>
                {exists && <span className="text-[10px] text-muted-foreground">configured</span>}
              </button>
            );
          })}
        </div>

        {selectedKind && meta && (
          <div className="space-y-3 rounded-lg border p-4">
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Bot Name
              </label>
              <Input
                placeholder={selectedKind === "telegram" ? "my_telegram_bot" : selectedKind === "discord" ? "my_discord_bot" : `my_${selectedKind}_bot`}
                value={connectorId}
                onChange={(e) => setConnectorId(e.target.value)}
                className="mt-1"
              />
              <p className="text-xs text-muted-foreground mt-1">A unique name to identify this bot, no spaces (e.g. support_bot, main_bot)</p>
            </div>
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Bot Token
              </label>
              <Input
                type="password"
                placeholder={selectedKind === "telegram" ? "123456:ABC-DEF..." : "Bot token from Developer Portal"}
                value={token}
                onChange={(e) => setToken(e.target.value)}
                className="mt-1"
              />
            </div>
            {meta.tokenLink && (
              <a
                href={meta.tokenLink}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1 text-xs text-primary hover:underline"
              >
                Get a bot token <ExternalLink className="h-3 w-3" />
              </a>
            )}
          </div>
        )}

        <DialogFooter>
          <Button
            onClick={handleSubmit}
            disabled={!selectedKind || !connectorId || !token || submitting}
          >
            {submitting ? <Loader2 className="h-4 w-4 animate-spin" /> : "Add Channel"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
export default function ChannelsPage() {
  const { data: channels, isLoading } = useChannels();
  const { data: statuses } = useChannelStatus();
  const updateChannels = useUpdateChannels();
  const removeConnector = useRemoveConnector();
  const [tokens, setTokens] = useState<Record<string, string>>({});
  const [restartRequired, setRestartRequired] = useState(false);

  const statusMap = new Map((statuses ?? []).map((item) => [`${item.kind}:${item.connector_id}`, item.status]));

  // Dynamic channel keys: merge known meta + any keys from backend
  const channelKeys = Array.from(new Set([
    ...Object.keys(CHANNEL_META),
    ...Object.keys(channels ?? {}),
  ]));

  // Only consider a channel kind as "existing" if it has at least one connector
  const existingKinds = new Set(
    Object.entries(channels ?? {})
      .filter(([, ch]) => ch.connectors && ch.connectors.length > 0)
      .map(([kind]) => kind)
  );

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

  return (
    <div className="space-y-6">
      <RestartBanner visible={restartRequired} />

      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Channels</h2>
          <p className="text-sm text-muted-foreground">Manage messaging platform connections.</p>
        </div>
        <AddChannelDialog existingKinds={existingKinds} onDone={() => setRestartRequired(true)} />
      </div>

      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {channelKeys.map((key) => {
        const meta = CHANNEL_META[key] ?? { label: key, description: "", color: "text-muted-foreground" };
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
