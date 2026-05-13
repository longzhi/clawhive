import { useState, useEffect } from "react";
import { QRCodeSVG } from "qrcode.react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { MessageCircle, Loader2, Key, Trash2, Plus, ExternalLink } from "lucide-react";
import { type ConnectorConfig, useChannelStatus, useChannels, useRemoveConnector, useUpdateChannels, useAddConnector } from "@/hooks/use-api";
import { toast } from "sonner";
import { AddConnectorDialog } from "@/components/channels/add-connector-dialog";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";

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

const CHANNEL_META: Record<string, { label: string; description: string; color: string; tokenLink: string }> = {
  telegram: { label: "Telegram", description: "Bot API integration", color: "text-blue-500", tokenLink: "https://t.me/BotFather" },
  discord: { label: "Discord", description: "Gateway connection", color: "text-indigo-500", tokenLink: "https://discord.com/developers/applications" },
  slack: { label: "Slack", description: "Workspace bot", color: "text-purple-500", tokenLink: "https://api.slack.com/apps" },
  whatsapp: { label: "WhatsApp", description: "Business API", color: "text-green-500", tokenLink: "https://developers.facebook.com/apps/" },
  imessage: { label: "iMessage", description: "Apple Messages", color: "text-sky-500", tokenLink: "" },
  feishu: { label: "Feishu", description: "Feishu Bot (WebSocket)", color: "text-cyan-500", tokenLink: "https://open.feishu.cn/app" },
  dingtalk: { label: "DingTalk", description: "DingTalk Bot (Stream)", color: "text-orange-500", tokenLink: "https://open-dev.dingtalk.com/" },
  wecom: { label: "WeCom", description: "WeCom AI Bot", color: "text-green-600", tokenLink: "https://developer.work.weixin.qq.com/" },
  weixin: { label: "WeChat", description: "Personal account via iLink", color: "text-emerald-500", tokenLink: "" },
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
  const [appId, setAppId] = useState("");
  const [appSecret, setAppSecret] = useState("");
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [botIdField, setBotIdField] = useState("");
  const [secretField, setSecretField] = useState("");
  const [dmPolicy, setDmPolicy] = useState<"allowlist" | "open">("allowlist");
  const [allowFromField, setAllowFromField] = useState("");
  const updateChannels = useUpdateChannels();
  const addConnector = useAddConnector();
  const { data: channels } = useChannels();
  const [submitting, setSubmitting] = useState(false);

  const reset = () => {
    setSelectedKind(null);
    setConnectorId("");
    setToken("");
    setAppId("");
    setAppSecret("");
    setClientId("");
    setClientSecret("");
    setBotIdField("");
    setSecretField("");
    setDmPolicy("allowlist");
    setAllowFromField("");
  };

  const handleSubmit = async () => {
    if (!selectedKind || !connectorId) return;
    const isChineseChannel = ["feishu", "dingtalk", "wecom"].includes(selectedKind);
    const isQrChannel = selectedKind === "weixin" || selectedKind === "whatsapp";
    if (!isChineseChannel && !isQrChannel && !token) return;
    setSubmitting(true);
    try {
      const current = channels ?? {};
      if (!current[selectedKind]) {
        const merged = { ...current, [selectedKind]: { enabled: true, connectors: [] } };
        await updateChannels.mutateAsync(merged);
      }
      await addConnector.mutateAsync({
        kind: selectedKind,
        connectorId,
        ...(token ? { token } : {}),
        ...(selectedKind === "feishu" ? { appId, appSecret } : {}),
        ...(selectedKind === "dingtalk" ? { clientId, clientSecret } : {}),
        ...(selectedKind === "wecom" ? { botId: botIdField, secret: secretField } : {}),
        ...(selectedKind === "telegram" ? { dmPolicy } : {}),
        ...(selectedKind === "telegram" && dmPolicy === "allowlist" && allowFromField
          ? { allowFrom: allowFromField.split(",").map(s => s.trim()).filter(Boolean) }
          : {}),
        ...(selectedKind === "whatsapp" ? { dmPolicy } : {}),
        ...(selectedKind === "whatsapp" && dmPolicy === "allowlist" && allowFromField
          ? { allowFrom: allowFromField.split(",").map(s => s.trim()).filter(Boolean) }
          : {}),
      });
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
                placeholder={`my_${selectedKind}_bot`}
                value={connectorId}
                onChange={(e) => setConnectorId(e.target.value)}
                className="mt-1"
              />
              <p className="text-xs text-muted-foreground mt-1">A unique name to identify this bot, no spaces</p>
            </div>

            {selectedKind === "feishu" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">App ID</label>
                  <Input placeholder="cli_xxx" value={appId} onChange={(e) => setAppId(e.target.value)} className="mt-1" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">App Secret</label>
                  <Input type="password" placeholder="App secret from Feishu Developer Console" value={appSecret} onChange={(e) => setAppSecret(e.target.value)} className="mt-1" />
                </div>
              </>
            ) : selectedKind === "dingtalk" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Client ID</label>
                  <Input placeholder="AppKey from DingTalk" value={clientId} onChange={(e) => setClientId(e.target.value)} className="mt-1" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Client Secret</label>
                  <Input type="password" placeholder="AppSecret from DingTalk" value={clientSecret} onChange={(e) => setClientSecret(e.target.value)} className="mt-1" />
                </div>
              </>
            ) : selectedKind === "wecom" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Bot ID</label>
                  <Input placeholder="Bot ID from WeCom Admin" value={botIdField} onChange={(e) => setBotIdField(e.target.value)} className="mt-1" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Secret</label>
                  <Input type="password" placeholder="Bot secret" value={secretField} onChange={(e) => setSecretField(e.target.value)} className="mt-1" />
                </div>
              </>
            ) : selectedKind === "weixin" ? (
              <p className="text-sm text-muted-foreground">
                No credentials needed. After adding, click the QR login button to scan with WeChat.
              </p>
            ) : selectedKind === "whatsapp" ? (
              <>
                <p className="text-sm text-muted-foreground">
                  No credentials needed. After adding, use the Pair Device button to scan the QR code with WhatsApp.
                </p>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">DM Access Policy</label>
                  <div className="mt-1.5 flex gap-2">
                    {([
                      { label: "Allowlist (recommended)", value: "allowlist" as const },
                      { label: "Open", value: "open" as const },
                    ]).map((opt) => (
                      <button
                        key={opt.value}
                        onClick={() => setDmPolicy(opt.value)}
                        className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                          dmPolicy === opt.value
                            ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                            : "border-border hover:border-primary/40"
                        }`}
                      >
                        {opt.label}
                      </button>
                    ))}
                  </div>
                  {dmPolicy === "open" && (
                    <p className="text-xs text-amber-600 mt-1">Anyone who messages your number can chat with the bot.</p>
                  )}
                </div>
                {dmPolicy === "allowlist" && (
                  <div>
                    <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Allowed Phone Numbers</label>
                    <Input
                      placeholder="+1234567890, +0987654321"
                      value={allowFromField}
                      onChange={(e) => setAllowFromField(e.target.value)}
                      className="mt-1"
                    />
                    <p className="text-xs text-muted-foreground mt-1">Comma-separated phone numbers with country code.</p>
                  </div>
                )}
              </>
            ) : (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Bot Token</label>
                <Input type="password" placeholder={selectedKind === "telegram" ? "123456:ABC-DEF..." : "Bot token from Developer Portal"} value={token} onChange={(e) => setToken(e.target.value)} className="mt-1" />
              </div>
            )}

            {selectedKind === "telegram" && (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">DM Access Policy</label>
                  <div className="mt-1.5 flex gap-2">
                    {([
                      { label: "Allowlist (recommended)", value: "allowlist" as const },
                      { label: "Open", value: "open" as const },
                    ]).map((opt) => (
                      <button
                        key={opt.value}
                        onClick={() => setDmPolicy(opt.value)}
                        className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                          dmPolicy === opt.value
                            ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                            : "border-border hover:border-primary/40"
                        }`}
                      >
                        {opt.label}
                      </button>
                    ))}
                  </div>
                  {dmPolicy === "open" && (
                    <p className="text-xs text-amber-600 mt-1">Anyone who finds your bot can chat with it.</p>
                  )}
                </div>
                {dmPolicy === "allowlist" && (
                  <div>
                    <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Allowed User IDs</label>
                    <Input
                      placeholder="Your Telegram user ID (comma-separated)"
                      value={allowFromField}
                      onChange={(e) => setAllowFromField(e.target.value)}
                      className="mt-1"
                    />
                    <p className="text-xs text-muted-foreground mt-1">Use @userinfobot on Telegram to find your ID.</p>
                  </div>
                )}
              </>
            )}

            {meta.tokenLink && (
              <a href={meta.tokenLink} target="_blank" rel="noopener noreferrer" className="flex items-center gap-1 text-xs text-primary hover:underline">
                Get credentials <ExternalLink className="h-3 w-3" />
              </a>
            )}
          </div>
        )}

        <DialogFooter>
          <Button
            onClick={handleSubmit}
            disabled={!selectedKind || !connectorId || submitting || (() => {
              if (selectedKind === "feishu") return !appId || !appSecret;
              if (selectedKind === "dingtalk") return !clientId || !clientSecret;
              if (selectedKind === "wecom") return !botIdField || !secretField;
              if (selectedKind === "weixin") return false;
              if (selectedKind === "whatsapp") return false;
              return !token;
            })()}
          >
            {submitting ? <Loader2 className="h-4 w-4 animate-spin" /> : "Add Channel"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// WeChat QR Login Component
// ---------------------------------------------------------------------------
function WeixinQrLogin({ connectorId, status }: { connectorId: string; status: string }) {
  const [qrUrl, setQrUrl] = useState<string | null>(null);
  const [qrToken, setQrToken] = useState<string | null>(null);
  const [polling, setPolling] = useState(false);
  const [loginStatus, setLoginStatus] = useState<string | null>(null);

  const startQrLogin = async () => {
    setLoginStatus(null);
    try {
      const res = await fetch("/api/channels/weixin/qr-login", { method: "POST" });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      setQrUrl(data.qrcode_url);
      setQrToken(data.qrcode_token);
      setPolling(true);
    } catch (e) {
      toast.error("Failed to get QR code");
    }
  };

  useEffect(() => {
    if (!polling || !qrToken) return;
    let cancelled = false;
    const poll = async () => {
      while (!cancelled) {
        try {
          const res = await fetch(`/api/channels/weixin/qr-status?token=${encodeURIComponent(qrToken)}&connector_id=${encodeURIComponent(connectorId)}`);
          if (!res.ok) break;
          const data = await res.json();
          if (data.status === "confirmed") {
            setLoginStatus("confirmed");
            setPolling(false);
            setQrUrl(null);
            toast.success(`WeChat logged in as ${data.bot_id ?? "unknown"}`);
            return;
          }
          if (data.status === "expired") {
            setLoginStatus("expired");
            setPolling(false);
            setQrUrl(null);
            toast.error("QR code expired, try again");
            return;
          }
          // "wait" or "scaned" — keep polling
          await new Promise(r => setTimeout(r, 2000));
        } catch {
          break;
        }
      }
    };
    poll();
    return () => { cancelled = true; };
  }, [polling, qrToken, connectorId]);

  if (status === "connected" && !qrUrl && !polling) {
    return (
      <div className="flex items-center gap-2">
        <Badge variant="outline" className="w-fit text-[10px] text-green-600 border-green-200 bg-green-50">
          Session active
        </Badge>
        <Button size="sm" variant="ghost" className="h-6 text-[10px] px-2" onClick={startQrLogin}>
          Re-login
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      {qrUrl ? (
        <div className="flex flex-col items-center gap-2 py-2">
          <div className="rounded-lg border p-3 bg-white">
            <QRCodeSVG value={qrUrl} size={176} />
          </div>
          <p className="text-xs text-muted-foreground">
            {polling ? "Waiting for scan..." : loginStatus === "confirmed" ? "Logged in!" : "Scan with WeChat"}
          </p>
          {polling && <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />}
        </div>
      ) : (
        <Button size="sm" variant="outline" className="h-8 gap-1.5" onClick={startQrLogin}>
          <Key className="h-3.5 w-3.5" />
          QR Login
        </Button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// WhatsApp QR Pair Component
// ---------------------------------------------------------------------------
function WhatsAppQrPair({ connectorId, status }: { connectorId: string; status: string }) {
  const [qrData, setQrData] = useState<string | null>(null);
  const [polling, setPolling] = useState(false);
  const [pairStatus, setPairStatus] = useState<string | null>(null);
  const pairingFailed = pairStatus === "failed" || pairStatus === "expired";

  const startPairing = async () => {
    setPairStatus(null);
    try {
      const res = await fetch(`/api/channels/whatsapp/qr-pair?connector_id=${encodeURIComponent(connectorId)}`, { method: "POST" });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      setPolling(true);
    } catch {
      toast.error("Failed to start WhatsApp pairing");
    }
  };

  useEffect(() => {
    if (!polling) return;
    let cancelled = false;
    const poll = async () => {
      while (!cancelled) {
        try {
          const res = await fetch(`/api/channels/whatsapp/qr-status?connector_id=${encodeURIComponent(connectorId)}`);
          if (!res.ok) break;
          const data = await res.json();

          if (data.status === "qr_ready" && data.qr_data) {
            setQrData(data.qr_data);
          }
          if (data.status === "paired") {
            setPairStatus("paired");
            setPolling(false);
            setQrData(null);
            toast.success("WhatsApp paired successfully!");
            return;
          }
          if (data.status === "already_paired") {
            setPairStatus("already_paired");
            setPolling(false);
            setQrData(null);
            toast.success("WhatsApp already paired");
            return;
          }
          if (data.status === "failed" || data.status === "expired") {
            setPairStatus(data.status);
            setPolling(false);
            setQrData(null);
            toast.error("WhatsApp pairing failed, try again");
            return;
          }
          await new Promise(r => setTimeout(r, 2000));
        } catch {
          break;
        }
      }
    };
    poll();
    return () => { cancelled = true; };
  }, [polling, connectorId]);

  if (status === "connected" && !qrData && !polling) {
    return (
      <div className="flex items-center gap-2">
        <Badge variant="outline" className="w-fit text-[10px] text-green-600 border-green-200 bg-green-50">
          Session active
        </Badge>
        <Button size="sm" variant="ghost" className="h-6 text-[10px] px-2" onClick={startPairing}>
          Re-pair
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      {qrData ? (
        <div className="flex flex-col items-center gap-2 py-2">
          <div className="rounded-lg border p-3 bg-white">
            <QRCodeSVG value={qrData} size={176} />
          </div>
          <p className="text-xs text-muted-foreground">
            {polling ? "Scan with WhatsApp \u2192 Linked Devices \u2192 Link a Device" : pairStatus === "paired" ? "Paired!" : "Waiting..."}
          </p>
          {polling && <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />}
        </div>
      ) : (
        <>
          <Button size="sm" variant="outline" className="h-8 gap-1.5" onClick={startPairing} disabled={polling}>
            <Key className="h-3.5 w-3.5" />
            {polling ? "Connecting..." : "Pair Device"}
          </Button>
          {pairingFailed && (
            <p className="text-xs text-destructive">
              Pairing {pairStatus}. Try again.
            </p>
          )}
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Channels Skeleton
// ---------------------------------------------------------------------------
function ChannelsSkeleton() {
  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <Skeleton className="h-6 w-24" />
          <Skeleton className="h-4 w-64 mt-1" />
        </div>
        <Skeleton className="h-9 w-32" />
      </div>
      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {Array.from({ length: 3 }).map((_, i) => (
          <Card key={i}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <div className="flex flex-col space-y-1">
                <Skeleton className="h-5 w-28" />
                <Skeleton className="h-3 w-40" />
              </div>
            </CardHeader>
            <CardContent className="pt-4">
              <Skeleton className="h-4 w-full" />
              <Skeleton className="h-4 w-3/4 mt-2" />
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
export default function ChannelsPage() {
  const { data: channels, isLoading, isError, error, refetch } = useChannels();
  const { data: statuses, dataUpdatedAt: statusesUpdatedAt } = useChannelStatus();

  const updateChannels = useUpdateChannels();
  const removeConnector = useRemoveConnector();
  const [tokens, setTokens] = useState<Record<string, string>>({});
  const [deleteTarget, setDeleteTarget] = useState<{ kind: string; id: string } | null>(null);

  const statusMap = new Map((statuses ?? []).map((item) => [`${item.kind}:${item.connector_id}`, item.status]));

  const channelKeys = Object.entries(channels ?? {})
    .filter(([, ch]) => ch != null)
    .map(([kind]) => kind);

  // Only consider a channel kind as "existing" if it has at least one connector
  const existingKinds = new Set(
    Object.entries(channels ?? {})
      .filter(([, ch]) => ch?.connectors && ch.connectors.length > 0)
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

  const handleRemoveConnector = async (channelKey: string, connectorId: string) => {
    try {
      await removeConnector.mutateAsync({ kind: channelKey, connectorId });
      toast.success("Connector removed");
    } catch {
      toast.error("Failed to remove connector");
    }
  };

  if (isLoading) return <ChannelsSkeleton />;
  if (isError) return <ErrorState message={error?.message} onRetry={refetch} />;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Channels</h2>
          <p className="text-sm text-muted-foreground">Manage messaging platform connections.</p>
        </div>
        <div className="flex items-center gap-4">
          <UpdatedAgo dataUpdatedAt={statusesUpdatedAt} />
          <AddChannelDialog existingKinds={existingKinds} onDone={() => {}} />
        </div>
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
                <AddConnectorDialog kind={key} label={meta.label} onAdded={() => {}} />
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
                            onClick={() => setDeleteTarget({ kind: key, id: connector.connector_id })}
                            disabled={removeConnector.isPending}
                          >
                            <Trash2 className="h-4 w-4" />
                            <span className="sr-only">Delete connector</span>
                          </Button>
                        </div>
                      </div>
                      {key === "weixin" ? (
                        <WeixinQrLogin connectorId={connector.connector_id} status={runtimeStatus} />
                      ) : key === "whatsapp" ? (
                        <WhatsAppQrPair connectorId={connector.connector_id} status={runtimeStatus} />
                      ) : (
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
                      )}
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
      <ConfirmDialog
        open={!!deleteTarget}
        onOpenChange={(open) => !open && setDeleteTarget(null)}
        title="Remove Connector"
        description={`Remove connector '${deleteTarget?.id}'? This cannot be undone.`}
        confirmLabel="Remove"
        variant="destructive"
        loading={removeConnector.isPending}
        onConfirm={() => {
          if (!deleteTarget) return;
          removeConnector.mutate(
            { kind: deleteTarget.kind, connectorId: deleteTarget.id },
            {
              onSuccess: () => {
                toast.success("Connector removed");
                setDeleteTarget(null);
              },
              onError: () => toast.error("Failed to remove connector"),
            }
          );
        }}
      />
    </div>
  );
}
