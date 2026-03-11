import { useState, useEffect, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { OpenAiOAuthSetup } from "@/components/providers/openai-oauth-setup";
import {
  useSetupStatus,
  useCreateProvider,
  useCreateAgent,
  useAddConnector,
  useRestart,
  useUpdateWebSearch,
  useActionbookConfig,
  useUpdateActionbook,
  useProviderPresets,
  useRouting,
  useUpdateRouting,
  useSetPassword,
  useListModels,
  useAuthStatus,
} from "@/hooks/use-api";
import type { ProviderPreset, ModelInfoResponse, ModelPresetInfo } from "@/hooks/use-api";
import {
  CheckCircle2,
  ChevronRight,
  ChevronLeft,
  ExternalLink,
  Loader2,
  RefreshCw,
  Zap,
} from "lucide-react";

import { toast } from "sonner";

const STEP_LABELS = ["Provider", "Agent", "Channel", "Tools", "Launch"];


// ---------------------------------------------------------------------------
// Security Setup Component
// ---------------------------------------------------------------------------
function SecuritySetup() {
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [passwordSet, setPasswordSet] = useState(false);
  const setPasswordMutation = useSetPassword();

  const handleSetPassword = () => {
    if (password !== confirmPassword) {
      toast.error("Passwords do not match");
      return;
    }
    if (password.length < 6) {
      toast.error("Password must be at least 6 characters");
      return;
    }
    setPasswordMutation.mutate(password, {
      onSuccess: () => {
        setPasswordSet(true);
        toast.success("Admin password set successfully");
      },
      onError: (err) => toast.error(err.message || "Failed to set password"),
    });
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          🔒 Admin Password
          {passwordSet && <Badge variant="default" className="bg-green-600">Set</Badge>}
        </CardTitle>
        <CardDescription>
          Optionally set a password to protect the web console. You can skip this step.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {passwordSet ? (
          <p className="text-sm text-muted-foreground">
            ✅ Password configured. You'll need to log in to access the console after setup.
          </p>
        ) : (
          <div className="grid gap-3 max-w-sm">
            <div className="grid gap-1.5">
              <Label htmlFor="setup-password">Password</Label>
              <Input
                id="setup-password"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Enter password"
              />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="setup-confirm">Confirm Password</Label>
              <Input
                id="setup-confirm"
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                placeholder="Confirm password"
              />
            </div>
            <Button
              onClick={handleSetPassword}
              disabled={!password || !confirmPassword || setPasswordMutation.isPending}
              className="w-fit"
            >
              {setPasswordMutation.isPending ? "Setting..." : "Set Password"}
            </Button>
            <p className="text-xs text-muted-foreground">
              Leave blank and click Next to skip. No password = no login required.
            </p>
          </div>
        )}
      </CardContent>
    </Card>
  );
}


// ---------------------------------------------------------------------------
// Main Setup Wizard
// ---------------------------------------------------------------------------
export default function SetupPage() {
  const navigate = useNavigate();
  const { data: setupStatus, isLoading: statusLoading } = useSetupStatus();
  const [step, setStep] = useState(0);
  const [wizardActive, setWizardActive] = useState(false);

  // Step 1: Provider
  const [selectedProvider, setSelectedProvider] = useState<ProviderPreset | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [providerCreated, setProviderCreated] = useState(false);

  // Step 2: Agent
  const [agentName, setAgentName] = useState("Clawhive");
  const [agentEmoji, setAgentEmoji] = useState("\u{1F980}");
  const [selectedModel, setSelectedModel] = useState("");
  const [agentCreated, setAgentCreated] = useState(false);
  const [agentId, setAgentId] = useState("clawhive-main");
  const [thinkingLevel, setThinkingLevel] = useState<string>("none");
  const [fetchedModels, setFetchedModels] = useState<ModelInfoResponse[]>([]);

  // Step 3: Channel
  const [channelKind, setChannelKind] = useState<"telegram" | "discord" | "feishu" | "dingtalk" | "wecom" | "slack" | "whatsapp" | "imessage" | null>(null);
  const [channelToken, setChannelToken] = useState("");
  const [channelConnectorId, setChannelConnectorId] = useState("");
  const [channelGroups, setChannelGroups] = useState("");
  const [channelAppId, setChannelAppId] = useState("");
  const [channelAppSecret, setChannelAppSecret] = useState("");
  const [channelClientId, setChannelClientId] = useState("");
  const [channelClientSecret, setChannelClientSecret] = useState("");
  const [channelBotId, setChannelBotId] = useState("");
  const [channelSecret, setChannelSecret] = useState("");
  const [channelRequireMention, setChannelRequireMention] = useState(true);
  const [channelRoutingKinds, setChannelRoutingKinds] = useState<("dm" | "group")[]>(["dm", "group"]);
  const [channelCreated, setChannelCreated] = useState(false);

  // Step 4: Web Search
  const [wsEnabled, setWsEnabled] = useState(false);
  const [wsProvider, setWsProvider] = useState("tavily");
  const [wsApiKey, setWsApiKey] = useState("");
  const [wsSaved, setWsSaved] = useState(false);

  const [abEnabled, setAbEnabled] = useState(false);
  const [abSaved, setAbSaved] = useState(false);

  // Step 5: Launch
  const [restarting, setRestarting] = useState(false);

  const createProvider = useCreateProvider();
  const createAgent = useCreateAgent();
  const addConnector = useAddConnector();
  const updateWebSearch = useUpdateWebSearch();
  const { data: abConfig } = useActionbookConfig();
  const updateActionbook = useUpdateActionbook();
  const restart = useRestart();
  const { data: providerPresets } = useProviderPresets();
  const { data: authStatus } = useAuthStatus();
  const { data: routingData } = useRouting(step === 2);
  const updateRouting = useUpdateRouting();
  const listModels = useListModels();
  const openAiOAuthProfileName =
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth" && profile.active)?.name ??
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth")?.name ??
    "openai-oauth";
  const hasOpenAiOAuth =
    authStatus?.profiles.some((profile) => profile.kind === "OpenAiOAuth") ?? false;

  // Mark wizard as active once we start interacting
  useEffect(() => {
    if (setupStatus?.needs_setup) {
      setWizardActive(true);
    }
  }, [setupStatus]);

  // Only redirect if not in the middle of wizard
  useEffect(() => {
    if (setupStatus && !setupStatus.needs_setup && !wizardActive) {
      navigate("/", { replace: true });
    }
  }, [setupStatus, navigate, wizardActive]);

  // Set defaults when provider is selected
  useEffect(() => {
    if (selectedProvider) {
      setApiBase(selectedProvider.api_base);
      if (selectedProvider.models.length > 0) {
        setSelectedModel(selectedProvider.models[0].id);
      }
    }
  }, [selectedProvider]);

  const canAdvance = useCallback(() => {
    switch (step) {
      case 0: return providerCreated;
      case 1: return agentCreated;
      case 2: return true; // Channel is optional
      case 3: return true; // Web Search is optional
      case 4: return false;
      default: return false;
    }
  }, [step, providerCreated, agentCreated]);

  const handleCreateProvider = async () => {
    if (!selectedProvider) return;
    try {
      await createProvider.mutateAsync({
        provider_id: selectedProvider.id,
        api_base: apiBase || selectedProvider.api_base,
        api_key: selectedProvider.needs_key ? apiKey : undefined,
        auth_profile:
          selectedProvider.id === "openai-chatgpt"
            ? openAiOAuthProfileName
            : undefined,
        models: [selectedProvider.default_model],
      });
      setProviderCreated(true);
    } catch {
      // error is handled by mutation state
    }
  };

  const handleFetchModels = async () => {
    if (!selectedProvider) return;
    try {
      const result = await listModels.mutateAsync({
        provider_type: selectedProvider.id,
        api_key: selectedProvider.needs_key ? apiKey : undefined,
        base_url: apiBase || undefined,
      });
      setFetchedModels(result.models);
      if (result.models.length > 0 && !selectedModel) {
        setSelectedModel(result.models[0].id);
      }
      toast.success(`Fetched ${result.models.length} models`);
    } catch {
      toast.error("Failed to fetch models from provider");
    }
  };

  const handleCreateAgent = async () => {
    if (!selectedModel) return;
    try {
      await createAgent.mutateAsync({
        agent_id: agentId,
        name: agentName || "Clawhive",
        emoji: agentEmoji || "\u{1F980}",
        primary_model: selectedModel,
        ...(thinkingLevel !== "none" ? { thinking_level: thinkingLevel } : {}),
      });
      setAgentCreated(true);
    } catch {
      // error is handled by mutation state
    }
  };

  const handleAddChannel = async () => {
    if (!channelKind || !channelConnectorId) return;
    const isChineseChannel = ["feishu", "dingtalk", "wecom"].includes(channelKind);
    if (!isChineseChannel && !channelToken) return;
    const groups = channelGroups
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    const hasGroup = channelRoutingKinds.includes("group");
    try {
      await addConnector.mutateAsync({
        kind: channelKind,
        connectorId: channelConnectorId,
        ...(channelToken ? { token: channelToken } : {}),
        ...(channelKind === "feishu" ? { appId: channelAppId, appSecret: channelAppSecret } : {}),
        ...(channelKind === "dingtalk" ? { clientId: channelClientId, clientSecret: channelClientSecret } : {}),
        ...(channelKind === "wecom" ? { botId: channelBotId, secret: channelSecret } : {}),
        ...(channelKind === "discord" && hasGroup && groups.length > 0 ? { groups } : {}),
        ...(hasGroup ? { requireMention: channelRequireMention } : {}),
      });

      // Auto-create routing bindings
      const routeAgentId = agentId;
      const existing = (routingData as { default_agent_id?: string; bindings?: Array<Record<string, unknown>> }) ?? {};
      const bindings = [...(existing.bindings ?? [])];
      for (const kind of channelRoutingKinds) {
        bindings.push({
          channel_type: channelKind,
          connector_id: channelConnectorId,
          match: { kind },
          agent_id: routeAgentId,
        });
      }
      await updateRouting.mutateAsync({
        default_agent_id: existing.default_agent_id ?? routeAgentId,
        bindings,
      });

      setChannelCreated(true);
    } catch {
      // error is handled by mutation state
    }
  };

  const handleSaveWebSearch = async () => {
    if (!wsEnabled) {
      setWsSaved(true);
      return;
    }
    try {
      await updateWebSearch.mutateAsync({
        enabled: true,
        provider: wsProvider,
        api_key: wsApiKey || null,
      });
      setWsSaved(true);
    } catch {
      // error handled by mutation state
    }
  };

  const handleSaveActionbook = async () => {
    if (!abEnabled) {
      setAbSaved(true);
      return;
    }
    try {
      await updateActionbook.mutateAsync({ enabled: true });
      setAbSaved(true);
    } catch {
      // error handled by mutation state
    }
  };

  const handleLaunch = async () => {
    setRestarting(true);
    try {
      await restart.mutateAsync();
    } catch {
      // Expected — server will die
    }
    // Poll until server comes back
    const poll = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status");
        if (res.ok) {
          clearInterval(poll);
          navigate("/", { replace: true });
        }
      } catch {
        // Server still restarting
      }
    }, 2000);
  };

  if (statusLoading) {
    return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-background">
        <Loader2 className="h-8 w-8 animate-spin text-primary" />
      </div>
    );
  }

  return (
    <div className="fixed inset-0 z-50 bg-background overflow-auto">
      {/* Subtle background texture */}
      <div className="absolute inset-0 opacity-[0.03]" style={{
        backgroundImage: `radial-gradient(circle at 1px 1px, currentColor 1px, transparent 0)`,
        backgroundSize: "32px 32px",
      }} />

      <div className="relative mx-auto max-w-2xl px-6 py-12 md:py-20">
        {/* Header */}
        <div className="mb-12 text-center">
          <div className="mb-4 text-5xl" role="img" aria-label="bee">{"\u{1F41D}"}</div>
          <h1 className="text-2xl font-bold tracking-tight">Clawhive Setup</h1>
          <p className="mt-2 text-sm text-muted-foreground">
            Configure your AI agent in a few steps
          </p>
        </div>

        {/* Step indicator */}
        <div className="mb-10 flex items-center justify-center gap-1">
          {STEP_LABELS.map((label, i) => (
            <div key={label} className="flex items-center gap-1">
              <button
                onClick={() => {
                  if (i < step) setStep(i);
                }}
                disabled={i > step}
                className={`flex items-center gap-1.5 rounded-full px-3 py-1 text-xs font-medium transition-all ${
                  i === step
                    ? "bg-primary text-primary-foreground shadow-sm"
                    : i < step
                      ? "bg-primary/10 text-primary cursor-pointer hover:bg-primary/20"
                      : "bg-muted text-muted-foreground"
                }`}
              >
                {i < step ? (
                  <CheckCircle2 className="h-3 w-3" />
                ) : (
                  <span className="flex h-3 w-3 items-center justify-center text-[10px] font-bold">
                    {i + 1}
                  </span>
                )}
                {label}
              </button>
              {i < STEP_LABELS.length - 1 && (
                <ChevronRight className="h-3 w-3 text-muted-foreground/40" />
              )}
            </div>
          ))}
        </div>

        {/* Step content */}
        <div className="min-h-[360px]">
          {step === 0 && (
            <StepProvider
              providers={providerPresets ?? []}
              selected={selectedProvider}
              onSelect={setSelectedProvider}
              apiKey={apiKey}
              onApiKeyChange={setApiKey}
              apiBase={apiBase}
              onApiBaseChange={setApiBase}
              onSubmit={handleCreateProvider}
              isCreating={createProvider.isPending}
              isCreated={providerCreated}
              hasOpenAiOAuth={hasOpenAiOAuth}
              error={createProvider.error?.message}
            />
          )}
          {step === 1 && (
            <StepAgent
              agentId={agentId}
              onAgentIdChange={setAgentId}
              name={agentName}
              onNameChange={setAgentName}
              emoji={agentEmoji}
              onEmojiChange={setAgentEmoji}
              models={fetchedModels.length > 0 ? fetchedModels : (selectedProvider?.models ?? [])}
              selectedModel={selectedModel}
              onModelChange={setSelectedModel}
              thinkingLevel={thinkingLevel}
              onThinkingLevelChange={setThinkingLevel}
              onSubmit={handleCreateAgent}
              isCreating={createAgent.isPending}
              isCreated={agentCreated}
              error={createAgent.error?.message}
              onFetchModels={handleFetchModels}
              isFetchingModels={listModels.isPending}
              canFetchModels={
                providerCreated && selectedProvider?.id !== "openai-chatgpt"
              }
            />
          )}
          {step === 2 && (
            <StepChannel
              kind={channelKind}
              onKindChange={setChannelKind}
              token={channelToken}
              onTokenChange={setChannelToken}
              connectorId={channelConnectorId}
              onConnectorIdChange={setChannelConnectorId}
              groups={channelGroups}
              onGroupsChange={setChannelGroups}
              requireMention={channelRequireMention}
              onRequireMentionChange={setChannelRequireMention}
              routingKinds={channelRoutingKinds}
              onRoutingKindsChange={setChannelRoutingKinds}
              onSubmit={handleAddChannel}
              isCreating={addConnector.isPending || updateRouting.isPending}
              isCreated={channelCreated}
              error={addConnector.error?.message ?? updateRouting.error?.message}
              appId={channelAppId}
              onAppIdChange={setChannelAppId}
              appSecret={channelAppSecret}
              onAppSecretChange={setChannelAppSecret}
              clientId={channelClientId}
              onClientIdChange={setChannelClientId}
              clientSecret={channelClientSecret}
              onClientSecretChange={setChannelClientSecret}
              botId={channelBotId}
              onBotIdChange={setChannelBotId}
              secret={channelSecret}
              onSecretChange={setChannelSecret}
            />
          )}
          {step === 3 && (
            <div className="space-y-10">
              <StepWebSearch
                enabled={wsEnabled}
                onEnabledChange={setWsEnabled}
                provider={wsProvider}
                onProviderChange={setWsProvider}
                apiKey={wsApiKey}
                onApiKeyChange={setWsApiKey}
                onSubmit={handleSaveWebSearch}
                isSaving={updateWebSearch.isPending}
                isSaved={wsSaved}
                error={updateWebSearch.error?.message}
              />
              <div className="border-t pt-8">
                <div>
                  <h2 className="text-lg font-semibold">Browser Automation</h2>
                  <p className="text-sm text-muted-foreground mt-1">
                    Optional: enable browser automation so your agent can interact with web pages,
                    fill forms, and extract data. Requires the actionbook CLI tool.
                  </p>
                </div>
                <div className="mt-4 grid grid-cols-2 gap-3">
                  <button
                    onClick={() => { if (!abSaved) setAbEnabled(true); }}
                    disabled={abSaved}
                    className={`rounded-lg border px-4 py-4 text-left transition-all ${
                      abEnabled
                        ? "border-primary bg-primary/5 ring-1 ring-primary/20"
                        : "border-border hover:border-primary/40 hover:bg-muted/50"
                    } ${abSaved ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
                  >
                    <div className="text-sm font-medium">Enable</div>
                    <div className="mt-0.5 text-xs text-muted-foreground">Give your agent browser control</div>
                  </button>
                  <button
                    onClick={() => { if (!abSaved) setAbEnabled(false); }}
                    disabled={abSaved}
                    className={`rounded-lg border px-4 py-4 text-left transition-all ${
                      !abEnabled
                        ? "border-primary bg-primary/5 ring-1 ring-primary/20"
                        : "border-border hover:border-primary/40 hover:bg-muted/50"
                    } ${abSaved ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
                  >
                    <div className="text-sm font-medium">Skip</div>
                    <div className="mt-0.5 text-xs text-muted-foreground">No browser automation for now</div>
                  </button>
                </div>
                {abEnabled && (
                  <Card className="mt-4 border-primary/20 bg-primary/[0.02]">
                    <CardContent className="space-y-4">
                      <div className="flex items-center gap-2">
                        {abConfig?.installed ? (
                          <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                            <CheckCircle2 className="h-3.5 w-3.5" />
                            actionbook CLI installed
                          </span>
                        ) : (
                          <div className="space-y-2">
                            <p className="text-xs text-amber-600 font-medium">actionbook CLI not found</p>
                            <p className="text-xs text-muted-foreground">
                              Install it with:{" "}
                              <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px]">
                                curl -fsSL https://actionbook.dev/install.sh | bash
                              </code>
                            </p>
                          </div>
                        )}
                      </div>
                      <div className="flex items-center justify-end">
                        {abSaved ? (
                          <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                            <CheckCircle2 className="h-3.5 w-3.5" />
                            Saved
                          </span>
                        ) : (
                          <Button
                            size="sm"
                            onClick={handleSaveActionbook}
                            disabled={updateActionbook.isPending}
                          >
                            {updateActionbook.isPending ? (
                              <Loader2 className="h-3.5 w-3.5 animate-spin" />
                            ) : (
                              "Save"
                            )}
                          </Button>
                        )}
                      </div>
                      {updateActionbook.error?.message && (
                        <p className="text-xs text-destructive">{updateActionbook.error.message}</p>
                      )}
                    </CardContent>
                  </Card>
                )}
              </div>
            </div>
          )}
          {step === 4 && (
            <>
              <SecuritySetup />
              <StepLaunch
                provider={selectedProvider}
                agentName={agentName}
                agentEmoji={agentEmoji}
                model={selectedModel}
                channel={channelKind}
                onLaunch={handleLaunch}
                restarting={restarting}
              />
            </>
          )}
        </div>

        {/* Navigation */}
        <div className="mt-8 flex items-center justify-between">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setStep((s) => Math.max(0, s - 1))}
            disabled={step === 0}
          >
            <ChevronLeft className="h-4 w-4" />
            Back
          </Button>
          {step < 4 && (
            <Button
              size="sm"
              onClick={() => setStep((s) => s + 1)}
              disabled={!canAdvance()}
            >
              {(step === 2 || step === 3) ? (
                (step === 2 && channelCreated) || (step === 3 && (wsSaved || !wsEnabled) && (abSaved || !abEnabled)) ? "Next" : "Skip"
              ) : "Next"}
              <ChevronRight className="h-4 w-4" />
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 1: Provider
// ---------------------------------------------------------------------------
function StepProvider({
  providers,
  selected,
  onSelect,
  apiKey,
  onApiKeyChange,
  apiBase,
  onApiBaseChange,
  onSubmit,
  isCreating,
  isCreated,
  hasOpenAiOAuth,
  error,
}: {
  providers: ProviderPreset[];
  selected: ProviderPreset | null;
  onSelect: (p: ProviderPreset) => void;
  apiKey: string;
  onApiKeyChange: (v: string) => void;
  apiBase: string;
  onApiBaseChange: (v: string) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
  hasOpenAiOAuth: boolean;
  error?: string;
}) {
  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Choose your LLM provider</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Select the AI provider you want to use. You can add more later.
        </p>
      </div>

      <div className="grid grid-cols-3 gap-2">
        {providers.map((p) => (
          <button
            key={p.id}
            onClick={() => { if (!isCreated) onSelect(p); }}
            disabled={isCreated}
            className={`rounded-lg border px-3 py-2.5 text-left text-sm font-medium transition-all ${
              selected?.id === p.id
                ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                : "border-border hover:border-primary/40 hover:bg-muted/50"
            } ${isCreated ? "opacity-60 cursor-not-allowed" : "cursor-pointer"}`}
          >
            {p.name}
          </button>
        ))}
      </div>

        {selected && (
          <Card className="border-primary/20 bg-primary/[0.02]">
          <CardContent className="space-y-4">
            {selected.id === "openai-chatgpt" && (
              <OpenAiOAuthSetup />
            )}
            {selected.needs_key && (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  API Key
                </label>
                <Input
                  type="password"
                  placeholder={`Enter your ${selected.name} API key`}
                  value={apiKey}
                  onChange={(e) => onApiKeyChange(e.target.value)}
                  disabled={isCreated}
                  className="mt-1.5"
                />
              </div>
            )}

            {(selected.id === "ollama" || selected.needs_base_url) && (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  {selected.needs_base_url ? "API Endpoint URL" : "API URL"}
                </label>
                <Input
                  placeholder={selected.needs_base_url ? "https://your-resource.openai.azure.com/openai/v1" : "http://localhost:11434/v1"}
                  value={apiBase}
                  onChange={(e) => onApiBaseChange(e.target.value)}
                  disabled={isCreated}
                  className="mt-1.5 font-mono"
                />
                {selected.needs_base_url && (
                  <p className="text-xs text-muted-foreground mt-1">Your Azure OpenAI resource endpoint URL</p>
                )}
              </div>
            )}

            <div className="flex items-center justify-between">
              <p className="text-xs text-muted-foreground">
                Models: {selected.models.map(m => m.id).join(", ")}
              </p>
              {isCreated ? (
                <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                  <CheckCircle2 className="h-3.5 w-3.5" />
                  Saved
                </span>
              ) : (
                <Button
                  size="sm"
                  onClick={onSubmit}
                  disabled={
                    isCreating ||
                    (selected.needs_key && !apiKey) ||
                    (selected.needs_base_url &&
                      (!apiBase || apiBase.includes("<your-resource>"))) ||
                    (selected.id === "openai-chatgpt" && !hasOpenAiOAuth)
                  }
                >
                  {isCreating ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    "Save Provider"
                  )}
                </Button>
              )}
            </div>
            {error && (
              <p className="text-xs text-destructive">{error}</p>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 2: Agent
// ---------------------------------------------------------------------------
const EMOJI_OPTIONS = ["\u{1F980}", "\u{1F916}", "\u{1F9E0}", "\u{26A1}", "\u{1F680}", "\u{1F4A1}", "\u{1F33F}", "\u{1F525}"];

function StepAgent({
  agentId,
  onAgentIdChange,
  name,
  onNameChange,
  emoji,
  onEmojiChange,
  models,
  selectedModel,
  onModelChange,
  thinkingLevel,
  onThinkingLevelChange,
  onSubmit,
  isCreating,
  isCreated,
  error,
  onFetchModels,
  isFetchingModels,
  canFetchModels,
}: {
  agentId: string;
  onAgentIdChange: (v: string) => void;
  name: string;
  onNameChange: (v: string) => void;
  emoji: string;
  onEmojiChange: (v: string) => void;
  models: (ModelPresetInfo | ModelInfoResponse)[];
  selectedModel: string;
  onModelChange: (v: string) => void;
  thinkingLevel: string;
  onThinkingLevelChange: (v: string) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
  error?: string;
  onFetchModels: () => void;
  isFetchingModels: boolean;
  canFetchModels: boolean;
}) {
  const [customModel, setCustomModel] = useState(false);
  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Create your agent</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Give your AI assistant a name and personality.
        </p>
      </div>

      <div className="space-y-4">
        <div>
          <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Agent ID
          </label>
          <Input
            placeholder="clawhive-main"
            value={agentId}
            onChange={(e) => onAgentIdChange(e.target.value.replace(/\s/g, '-').toLowerCase())}
            disabled={isCreated}
            className="mt-1.5 font-mono"
          />
          <p className="text-xs text-muted-foreground mt-1">Unique identifier for this agent (no spaces)</p>
        </div>

        <div>
          <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Agent Name
          </label>
          <Input
            placeholder="Clawhive"
            value={name}
            onChange={(e) => onNameChange(e.target.value)}
            disabled={isCreated}
            className="mt-1.5"
          />
        </div>

        <div>
          <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Emoji
          </label>
          <div className="mt-1.5 flex gap-1.5">
            {EMOJI_OPTIONS.map((e) => (
              <button
                key={e}
                onClick={() => { if (!isCreated) onEmojiChange(e); }}
                disabled={isCreated}
                className={`flex h-9 w-9 items-center justify-center rounded-md text-lg transition-all ${
                  emoji === e
                    ? "bg-primary/10 ring-1 ring-primary/30 scale-110"
                    : "hover:bg-muted"
                } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
              >
                {e}
              </button>
            ))}
          </div>
        </div>

        <div>
          <div className="flex items-center justify-between">
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Model
            </label>
            {canFetchModels && !isCreated && (
              <button
                onClick={onFetchModels}
                disabled={isFetchingModels}
                className="flex items-center gap-1 text-xs text-primary hover:underline disabled:opacity-50"
              >
                {isFetchingModels ? (
                  <Loader2 className="h-3 w-3 animate-spin" />
                ) : (
                  <RefreshCw className="h-3 w-3" />
                )}
                {isFetchingModels ? "Fetching..." : "Fetch from API"}
              </button>
            )}
          </div>
          <div className="mt-1.5 flex flex-wrap gap-2">
            {models.map((m) => (
              <button
                key={m.id}
                onClick={() => { if (!isCreated) { setCustomModel(false); onModelChange(m.id); } }}
                disabled={isCreated}
                className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                  selectedModel === m.id && !customModel
                    ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                    : "border-border hover:border-primary/40"
                } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
              >
                <span className="block">{m.id}</span>
                <span className="block text-[10px] text-muted-foreground font-normal">
                  {[
                    m.context_window ? (m.context_window >= 1_000_000 ? `${(m.context_window / 1_000_000).toFixed(0)}M` : `${Math.round(m.context_window / 1000)}k`) : null,
                    m.reasoning ? "reasoning" : null,
                    m.vision ? "vision" : null,
                  ].filter(Boolean).join(" · ") || ""}
                </span>
              </button>
            ))}
            <button
              onClick={() => { if (!isCreated) { setCustomModel(true); onModelChange(""); } }}
              disabled={isCreated}
              className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                customModel
                  ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                  : "border-border hover:border-primary/40"
              } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
            >
              Custom…
            </button>
          </div>
          {customModel && (
            <Input
              placeholder="provider/model-name (e.g. openai/gpt-5)"
              value={selectedModel}
              onChange={(e) => onModelChange(e.target.value)}
              disabled={isCreated}
              className="mt-2 font-mono"
            />
          )}
        </div>

        <div>
          <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Thinking Level
          </label>
          <p className="text-xs text-muted-foreground mt-0.5 mb-1">
            For reasoning models (o-series, Claude with extended thinking). Leave as None for standard models.
          </p>
          <div className="grid grid-cols-4 gap-2 mt-1">
            {(["none", "low", "medium", "high"] as const).map((level) => (
              <button
                key={level}
                onClick={() => { if (!isCreated) onThinkingLevelChange(level); }}
                disabled={isCreated}
                className={`rounded-lg border px-3 py-2 text-sm transition-all ${
                  thinkingLevel === level
                    ? "border-primary bg-primary/5 ring-1 ring-primary/20"
                    : "border-border hover:border-primary/40 hover:bg-muted/50"
                } ${isCreated ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
              >
                {level === "none" ? "None" : level.charAt(0).toUpperCase() + level.slice(1)}
              </button>
            ))}
          </div>
        </div>

        <div className="flex items-center justify-between pt-2">
          <div className="flex items-center gap-2 text-sm">
            <span className="text-lg">{emoji}</span>
            <span className="font-medium">{name || "Clawhive"}</span>
            <span className="text-muted-foreground text-xs">/ {selectedModel}</span>
          </div>
          {isCreated ? (
            <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
              <CheckCircle2 className="h-3.5 w-3.5" />
              Created
            </span>
          ) : (
            <Button
              size="sm"
              onClick={onSubmit}
              disabled={isCreating || !selectedModel}
            >
              {isCreating ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                "Create Agent"
              )}
            </Button>
          )}
        </div>
        {error && (
          <p className="text-xs text-destructive">{error}</p>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 3: Channel (optional)
// ---------------------------------------------------------------------------
type RoutingKind = "dm" | "group";
const ROUTING_OPTIONS: { label: string; value: RoutingKind[] }[] = [
  { label: "DM only", value: ["dm"] },
  { label: "Group only", value: ["group"] },
  { label: "DM + Group", value: ["dm", "group"] },
];

function StepChannel({
  kind,
  onKindChange,
  token,
  onTokenChange,
  connectorId,
  onConnectorIdChange,
  groups,
  onGroupsChange,
  requireMention,
  onRequireMentionChange,
  routingKinds,
  onRoutingKindsChange,
  onSubmit,
  isCreating,
  isCreated,
  error,
  appId, onAppIdChange,
  appSecret, onAppSecretChange,
  clientId, onClientIdChange,
  clientSecret, onClientSecretChange,
  botId, onBotIdChange,
  secret, onSecretChange,
}: {
  kind: "telegram" | "discord" | "feishu" | "dingtalk" | "wecom" | "slack" | "whatsapp" | "imessage" | null;
  onKindChange: (v: "telegram" | "discord" | "feishu" | "dingtalk" | "wecom" | "slack" | "whatsapp" | "imessage") => void;
  token: string;
  onTokenChange: (v: string) => void;
  connectorId: string;
  onConnectorIdChange: (v: string) => void;
  groups: string;
  onGroupsChange: (v: string) => void;
  requireMention: boolean;
  onRequireMentionChange: (v: boolean) => void;
  routingKinds: RoutingKind[];
  onRoutingKindsChange: (v: RoutingKind[]) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
  error?: string;
  appId: string;
  onAppIdChange: (v: string) => void;
  appSecret: string;
  onAppSecretChange: (v: string) => void;
  clientId: string;
  onClientIdChange: (v: string) => void;
  clientSecret: string;
  onClientSecretChange: (v: string) => void;
  botId: string;
  onBotIdChange: (v: string) => void;
  secret: string;
  onSecretChange: (v: string) => void;
}) {
  const hasGroup = routingKinds.includes("group");

  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Connect a channel</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Optional: connect a messaging platform so your agent can chat there.
          You can always set this up later from the dashboard.
        </p>
      </div>

      <div className="grid grid-cols-2 sm:grid-cols-3 gap-3">
        {(["telegram", "discord", "slack", "whatsapp", "imessage", "feishu", "dingtalk", "wecom"] as const).map((ch) => (
          <button
            key={ch}
            onClick={() => { if (!isCreated) onKindChange(ch); }}
            disabled={isCreated}
            className={`rounded-lg border px-4 py-4 text-left transition-all ${
              kind === ch
                ? "border-primary bg-primary/5 ring-1 ring-primary/20"
                : "border-border hover:border-primary/40 hover:bg-muted/50"
            } ${isCreated ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
          >
            <div className="text-sm font-medium capitalize">{ch === "dingtalk" ? "DingTalk" : ch === "wecom" ? "WeCom" : ch === "imessage" ? "iMessage" : ch === "whatsapp" ? "WhatsApp" : ch}</div>
            <div className="mt-0.5 text-xs text-muted-foreground">
              {ch === "telegram" ? "Add a Telegram bot" : ch === "discord" ? "Add a Discord bot" : ch === "slack" ? "Add a Slack bot" : ch === "whatsapp" ? "Connect WhatsApp" : ch === "imessage" ? "Connect iMessage (macOS)" : ch === "feishu" ? "Add a Feishu bot" : ch === "dingtalk" ? "Add a DingTalk bot" : "Add a WeCom bot"}
            </div>
          </button>
        ))}
      </div>

      {kind && (
        <Card className="border-primary/20 bg-primary/[0.02]">
          <CardContent className="space-y-4">
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Bot Name
              </label>
              <Input
                placeholder={kind === "telegram" ? "my_telegram_bot" : kind === "discord" ? "my_discord_bot" : kind === "slack" ? "my_slack_bot" : kind === "whatsapp" ? "my_whatsapp_bot" : kind === "imessage" ? "my_imessage_bot" : `my_${kind}_bot`}
                value={connectorId}
                onChange={(e) => onConnectorIdChange(e.target.value)}
                disabled={isCreated}
                className="mt-1.5"
              />
              <p className="text-xs text-muted-foreground mt-1">A unique name to identify this bot, no spaces (e.g. support_bot)</p>
            </div>

            {kind === "imessage" || kind === "whatsapp" ? (
              <p className="text-xs text-muted-foreground">
                {kind === "imessage" ? "No credentials needed. Requires macOS with Full Disk Access." : "No credentials needed. WhatsApp will pair via QR code on first start."}
              </p>
            ) : kind === "feishu" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">App ID</label>
                  <Input placeholder="cli_xxx" value={appId} onChange={(e) => onAppIdChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">App Secret</label>
                  <Input type="password" placeholder="App secret from Feishu Developer Console" value={appSecret} onChange={(e) => onAppSecretChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
              </>
            ) : kind === "dingtalk" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Client ID</label>
                  <Input placeholder="AppKey from DingTalk" value={clientId} onChange={(e) => onClientIdChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Client Secret</label>
                  <Input type="password" placeholder="AppSecret from DingTalk" value={clientSecret} onChange={(e) => onClientSecretChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
              </>
            ) : kind === "wecom" ? (
              <>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Bot ID</label>
                  <Input placeholder="Bot ID from WeCom Admin" value={botId} onChange={(e) => onBotIdChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Secret</label>
                  <Input type="password" placeholder="Bot secret" value={secret} onChange={(e) => onSecretChange(e.target.value)} disabled={isCreated} className="mt-1.5" />
                </div>
              </>
            ) : (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  Bot Token
                </label>
                <Input
                  type="password"
                  placeholder={kind === "telegram" ? "123456:ABC-DEF..." : kind === "slack" ? "xoxb-..." : "Bot token from Discord Developer Portal"}
                  value={token}
                  onChange={(e) => onTokenChange(e.target.value)}
                  disabled={isCreated}
                  className="mt-1.5"
                />
              </div>
            )}

            {/* Message routing kind selector */}
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Message Routing
              </label>
              <div className="mt-1.5 flex gap-2">
                {ROUTING_OPTIONS.map((opt) => {
                  const selected = JSON.stringify(routingKinds) === JSON.stringify(opt.value);
                  return (
                    <button
                      key={opt.label}
                      onClick={() => { if (!isCreated) onRoutingKindsChange(opt.value); }}
                      disabled={isCreated}
                      className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                        selected
                          ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                          : "border-border hover:border-primary/40"
                      } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
                    >
                      {opt.label}
                    </button>
                  );
                })}
              </div>
            </div>

            {/* Groups: Discord + group routing only */}
            {kind === "discord" && hasGroup && (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  Group IDs (optional)
                </label>
                <Input
                  placeholder="Comma-separated Discord channel IDs, leave empty for all"
                  value={groups}
                  onChange={(e) => onGroupsChange(e.target.value)}
                  disabled={isCreated}
                  className="mt-1.5"
                />
                <p className="text-xs text-muted-foreground mt-1">Only respond in these channels. Empty = all channels.</p>
              </div>
            )}

            {/* Require @mention: any channel type with group routing */}
            {hasGroup && (
              <div className="flex items-center gap-3">
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  Require @mention
                </label>
                <button
                  onClick={() => { if (!isCreated) onRequireMentionChange(!requireMention); }}
                  disabled={isCreated}
                  className={`relative inline-flex h-5 w-9 items-center rounded-full transition-colors ${
                    requireMention ? "bg-primary" : "bg-muted-foreground/30"
                  } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
                >
                  <span className={`inline-block h-3.5 w-3.5 transform rounded-full bg-white transition-transform ${
                    requireMention ? "translate-x-4.5" : "translate-x-0.5"
                  }`} />
                </button>
                <span className="text-xs text-muted-foreground">
                  {requireMention ? "Bot responds only when @mentioned" : "Bot responds to all messages"}
                </span>
              </div>
            )}

            <div className="flex items-center justify-between">
              {kind !== "imessage" && kind !== "whatsapp" ? (
                <a
                  href={kind === "telegram" ? "https://t.me/BotFather" : kind === "discord" ? "https://discord.com/developers/applications" : kind === "slack" ? "https://api.slack.com/apps" : kind === "feishu" ? "https://open.feishu.cn/app" : kind === "dingtalk" ? "https://open-dev.dingtalk.com/" : "https://developer.work.weixin.qq.com/"}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="flex items-center gap-1 text-xs text-primary hover:underline"
                >
                  Get credentials <ExternalLink className="h-3 w-3" />
                </a>
              ) : <div />}
              {isCreated ? (
                <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                  <CheckCircle2 className="h-3.5 w-3.5" />
                  Added
                </span>
              ) : (
                <Button
                  size="sm"
                  onClick={onSubmit}
                  disabled={isCreating || !connectorId || (() => {
                    if (kind === "imessage" || kind === "whatsapp") return false;
                    if (kind === "feishu") return !appId || !appSecret;
                    if (kind === "dingtalk") return !clientId || !clientSecret;
                    if (kind === "wecom") return !botId || !secret;
                    return !token;
                  })()}
                >
                  {isCreating ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    "Add Channel"
                  )}
                </Button>
              )}
            </div>
            {error && (
              <p className="text-xs text-destructive">{error}</p>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 4: Web Search (optional)
// ---------------------------------------------------------------------------
const WS_PROVIDERS = ["tavily", "serper", "brave"];

function StepWebSearch({
  enabled,
  onEnabledChange,
  provider,
  onProviderChange,
  apiKey,
  onApiKeyChange,
  onSubmit,
  isSaving,
  isSaved,
  error,
}: {
  enabled: boolean;
  onEnabledChange: (v: boolean) => void;
  provider: string;
  onProviderChange: (v: string) => void;
  apiKey: string;
  onApiKeyChange: (v: string) => void;
  onSubmit: () => void;
  isSaving: boolean;
  isSaved: boolean;
  error?: string;
}) {
  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Web Search</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Optional: enable web search so your agent can look up information online.
          You can skip this and configure it later.
        </p>
      </div>

      <div className="grid grid-cols-2 gap-3">
        <button
          onClick={() => { if (!isSaved) onEnabledChange(true); }}
          disabled={isSaved}
          className={`rounded-lg border px-4 py-4 text-left transition-all ${
            enabled
              ? "border-primary bg-primary/5 ring-1 ring-primary/20"
              : "border-border hover:border-primary/40 hover:bg-muted/50"
          } ${isSaved ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
        >
          <div className="text-sm font-medium">Enable</div>
          <div className="mt-0.5 text-xs text-muted-foreground">Give your agent web access</div>
        </button>
        <button
          onClick={() => { if (!isSaved) onEnabledChange(false); }}
          disabled={isSaved}
          className={`rounded-lg border px-4 py-4 text-left transition-all ${
            !enabled
              ? "border-primary bg-primary/5 ring-1 ring-primary/20"
              : "border-border hover:border-primary/40 hover:bg-muted/50"
          } ${isSaved ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
        >
          <div className="text-sm font-medium">Skip</div>
          <div className="mt-0.5 text-xs text-muted-foreground">No web search for now</div>
        </button>
      </div>

      {enabled && (
        <Card className="border-primary/20 bg-primary/[0.02]">
          <CardContent className="space-y-4">
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Search Provider
              </label>
              <div className="mt-1.5 flex flex-wrap gap-2">
                {WS_PROVIDERS.map((p) => (
                  <button
                    key={p}
                    onClick={() => { if (!isSaved) onProviderChange(p); }}
                    disabled={isSaved}
                    className={`rounded-md border px-3 py-1.5 text-xs font-medium capitalize transition-all ${
                      provider === p
                        ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                        : "border-border hover:border-primary/40"
                    } ${isSaved ? "cursor-not-allowed" : "cursor-pointer"}`}
                  >
                    {p}
                  </button>
                ))}
              </div>
            </div>
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                API Key
              </label>
              <Input
                type="password"
                placeholder={`Enter your ${provider} API key`}
                value={apiKey}
                onChange={(e) => onApiKeyChange(e.target.value)}
                disabled={isSaved}
                className="mt-1.5"
              />
            </div>
            <div className="flex items-center justify-end">
              {isSaved ? (
                <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                  <CheckCircle2 className="h-3.5 w-3.5" />
                  Saved
                </span>
              ) : (
                <Button
                  size="sm"
                  onClick={onSubmit}
                  disabled={isSaving || !apiKey}
                >
                  {isSaving ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    "Save"
                  )}
                </Button>
              )}
            </div>
            {error && (
              <p className="text-xs text-destructive">{error}</p>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 5: Launch
// ---------------------------------------------------------------------------
function StepLaunch({
  provider,
  agentName,
  agentEmoji,
  model,
  channel,
  onLaunch,
  restarting,
}: {
  provider: ProviderPreset | null;
  agentName: string;
  agentEmoji: string;
  model: string;
  channel: string | null;
  onLaunch: () => void;
  restarting: boolean;
}) {
  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Ready to launch</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Review your configuration and launch clawhive.
        </p>
      </div>

      <Card>
        <CardContent className="space-y-3">
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Provider</span>
            <span className="text-sm font-medium">{provider?.name ?? "—"}</span>
          </div>
          <div className="h-px bg-border" />
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Agent</span>
            <span className="text-sm font-medium">{agentEmoji} {agentName}</span>
          </div>
          <div className="h-px bg-border" />
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Model</span>
            <span className="text-sm font-mono">{model}</span>
          </div>
          <div className="h-px bg-border" />
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-muted-foreground uppercase tracking-wide">Channel</span>
            <span className="text-sm font-medium capitalize">{channel ?? "None (dashboard only)"}</span>
          </div>
        </CardContent>
      </Card>

      <div className="flex justify-center pt-4">
        {restarting ? (
          <div className="flex flex-col items-center gap-3">
            <Loader2 className="h-8 w-8 animate-spin text-primary" />
            <p className="text-sm text-muted-foreground">
              Restarting clawhive with new configuration...
            </p>
          </div>
        ) : (
          <Button size="lg" onClick={onLaunch} className="gap-2 px-8">
            <Zap className="h-4 w-4" />
            Launch Clawhive
          </Button>
        )}
      </div>
    </div>
  );
}
