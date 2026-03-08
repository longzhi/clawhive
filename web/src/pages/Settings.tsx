import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import {
  useWebSearchConfig,
  useUpdateWebSearch,
  useActionbookConfig,
  useUpdateActionbook,
  useSetPassword,
} from "@/hooks/use-api";
import { useThemeStore } from "@/stores/theme";
import { Loader2, Search, BookOpen, Check, Sun, Moon, Monitor, Lock } from "lucide-react";
import { toast } from "sonner";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";

// ---------------------------------------------------------------------------
// Settings Skeleton
// ---------------------------------------------------------------------------
function SettingsSkeleton() {
  return (
    <div className="flex flex-col gap-6 max-w-2xl">
      {Array.from({ length: 2 }).map((_, i) => (
        <Card key={i}>
          <CardHeader>
            <Skeleton className="h-5 w-32" />
            <Skeleton className="h-4 w-48 mt-1" />
          </CardHeader>
          <CardContent className="space-y-4">
            <Skeleton className="h-9 w-full" />
            <Skeleton className="h-9 w-full" />
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Security Card — change console password
// ---------------------------------------------------------------------------
function SecurityCard() {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState("");
  const setPasswordMutation = useSetPassword();

  const handleSave = async () => {
    setError("");
    if (password.length < 6) {
      setError("Password must be at least 6 characters");
      return;
    }
    if (password !== confirm) {
      setError("Passwords do not match");
      return;
    }
    try {
      await setPasswordMutation.mutateAsync(password);
      setPassword("");
      setConfirm("");
      toast.success("Password updated");
    } catch {
      toast.error("Failed to update password");
    }
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          <Lock className="h-5 w-5 text-muted-foreground" />
          <div>
            <CardTitle className="text-base">Security</CardTitle>
            <CardDescription className="mt-1">Manage console access</CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid gap-2">
          <Label htmlFor="sec-password">New Password</Label>
          <Input
            id="sec-password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder="Enter new password (min 6 characters)"
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="sec-confirm">Confirm Password</Label>
          <Input
            id="sec-confirm"
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            placeholder="Confirm new password"
          />
        </div>
        {error && <p className="text-sm text-destructive">{error}</p>}
        <Button
          disabled={setPasswordMutation.isPending || !password || !confirm}
          onClick={handleSave}
        >
          {setPasswordMutation.isPending ? "Saving..." : "Update Password"}
        </Button>
      </CardContent>
    </Card>
  );
}

export default function SettingsPage() {
  const { data: webSearch, isLoading: isLoadingWS, isError: isErrorWS, refetch: refetchWS } = useWebSearchConfig();
  const updateWebSearch = useUpdateWebSearch();
  const { data: actionbook, isLoading: isLoadingAB, isError: isErrorAB, refetch: refetchAB } = useActionbookConfig();
  const updateActionbook = useUpdateActionbook();
  const { theme, setTheme } = useThemeStore();

  const [wsProvider, setWsProvider] = useState("");
  const [wsApiKey, setWsApiKey] = useState("");
  const [wsProviderDirty, setWsProviderDirty] = useState(false);

  // Sync provider from server on first load
  const effectiveProvider = wsProviderDirty ? wsProvider : (webSearch?.provider ?? "");

  if (isLoadingWS || isLoadingAB) return <SettingsSkeleton />;
  if (isErrorWS || isErrorAB) return <ErrorState message="Failed to load settings" onRetry={() => { void refetchWS(); void refetchAB(); }} />

  return (
    <div className="flex flex-col gap-6 max-w-2xl">
      {/* Web Search */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Search className="h-5 w-5 text-muted-foreground" />
              <div>
                <CardTitle className="text-base">Web Search</CardTitle>
                <CardDescription className="mt-1">Enable agents to search the web</CardDescription>
              </div>
            </div>
            <Switch
              checked={webSearch?.enabled ?? false}
              disabled={updateWebSearch.isPending}
              onCheckedChange={async (enabled) => {
                try {
                  await updateWebSearch.mutateAsync({
                    enabled,
                    provider: webSearch?.provider ?? null,
                    api_key: null,
                  });
                  toast.success(enabled ? "Web search enabled" : "Web search disabled");
                } catch {
                  toast.error("Failed to update web search");
                }
              }}
            />
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid gap-2">
            <Label htmlFor="ws-provider">Provider</Label>
            <div className="flex gap-2">
              <Input
                id="ws-provider"
                placeholder="e.g. tavily, brave, serper"
                value={effectiveProvider}
                onChange={(e) => {
                  setWsProvider(e.target.value);
                  setWsProviderDirty(true);
                }}
              />
              <Button
                variant="secondary"
                disabled={updateWebSearch.isPending || !wsProviderDirty}
                onClick={async () => {
                  try {
                    await updateWebSearch.mutateAsync({
                      enabled: webSearch?.enabled ?? false,
                      provider: wsProvider || null,
                      api_key: null,
                    });
                    setWsProviderDirty(false);
                    toast.success("Provider updated");
                  } catch {
                    toast.error("Failed to update provider");
                  }
                }}
              >
                Save
              </Button>
            </div>
          </div>

          <Separator />

          <div className="grid gap-2">
            <div className="flex items-center gap-2">
              <Label htmlFor="ws-apikey">API Key</Label>
              {webSearch?.has_api_key && (
                <Badge variant="outline" className="text-green-700 border-green-200 bg-green-50 text-[10px] h-5">
                  <Check className="h-3 w-3 mr-1" />
                  Configured
                </Badge>
              )}
            </div>
            <div className="flex gap-2">
              <Input
                id="ws-apikey"
                type="password"
                placeholder={webSearch?.has_api_key ? "••••••••" : "Enter API key"}
                value={wsApiKey}
                onChange={(e) => setWsApiKey(e.target.value)}
              />
              <Button
                variant="secondary"
                disabled={updateWebSearch.isPending || !wsApiKey.trim()}
                onClick={async () => {
                  try {
                    await updateWebSearch.mutateAsync({
                      enabled: webSearch?.enabled ?? false,
                      provider: webSearch?.provider ?? null,
                      api_key: wsApiKey,
                    });
                    setWsApiKey("");
                    toast.success("API key updated");
                  } catch {
                    toast.error("Failed to update API key");
                  }
                }}
              >
                Save
              </Button>
            </div>
          </div>
        </CardContent>
      </Card>

      {/* Actionbook */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <BookOpen className="h-5 w-5 text-muted-foreground" />
              <div>
                <CardTitle className="text-base">Actionbook</CardTitle>
                <CardDescription className="mt-1">Extended tool capabilities for agents</CardDescription>
              </div>
            </div>
            <Switch
              checked={actionbook?.enabled ?? false}
              disabled={updateActionbook.isPending}
              onCheckedChange={async (enabled) => {
                try {
                  await updateActionbook.mutateAsync({ enabled });
                  toast.success(enabled ? "Actionbook enabled" : "Actionbook disabled");
                } catch {
                  toast.error("Failed to update actionbook");
                }
              }}
            />
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">Installation status:</span>
            {actionbook?.installed ? (
              <Badge variant="outline" className="text-green-700 border-green-200 bg-green-50">
                <Check className="h-3 w-3 mr-1" />
                Installed
              </Badge>
            ) : (
              <Badge variant="outline" className="text-amber-700 border-amber-200 bg-amber-50">
                Not installed
              </Badge>
            )}
          </div>
        </CardContent>
      </Card>

      {/* Security */}
      <SecurityCard />

      {/* Appearance */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-2">
            <Sun className="h-5 w-5 text-muted-foreground" />
            <div>
              <CardTitle className="text-base">Appearance</CardTitle>
              <CardDescription className="mt-1">Customize the look of the console</CardDescription>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex gap-2">
            {([
              { value: "light" as const, label: "Light", icon: Sun },
              { value: "dark" as const, label: "Dark", icon: Moon },
              { value: "system" as const, label: "System", icon: Monitor },
            ]).map(({ value, label, icon: Icon }) => (
              <Button
                key={value}
                variant={theme === value ? "default" : "outline"}
                size="sm"
                onClick={() => setTheme(value)}
                className="flex items-center gap-2"
              >
                <Icon className="h-4 w-4" />
                {label}
              </Button>
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
