"use client";

import { useState, useEffect, useCallback } from "react";
import { useRouter } from "next/navigation";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent } from "@/components/ui/card";
import {
  useSetupStatus,
  useCreateProvider,
  useCreateAgent,
  useAddConnector,
  useRestart,
} from "@/hooks/use-api";
import { CheckCircle2, ChevronRight, ChevronLeft, Loader2, Zap, ExternalLink } from "lucide-react";

// ---------------------------------------------------------------------------
// Provider metadata — mirrors setup.rs ProviderId
// ---------------------------------------------------------------------------
interface ProviderMeta {
  id: string;
  name: string;
  apiBase: string;
  needsKey: boolean;
  defaultModels: string[];
}

const PROVIDERS: ProviderMeta[] = [
  { id: "anthropic", name: "Anthropic", apiBase: "https://api.anthropic.com/v1", needsKey: true, defaultModels: ["claude-sonnet-4-6", "claude-haiku-4-5"] },
  { id: "openai", name: "OpenAI", apiBase: "https://api.openai.com/v1", needsKey: true, defaultModels: ["gpt-4o", "gpt-4o-mini"] },
  { id: "gemini", name: "Google Gemini", apiBase: "https://generativelanguage.googleapis.com/v1beta", needsKey: true, defaultModels: ["gemini-2.5-pro", "gemini-2.5-flash"] },
  { id: "deepseek", name: "DeepSeek", apiBase: "https://api.deepseek.com/v1", needsKey: true, defaultModels: ["deepseek-chat", "deepseek-reasoner"] },
  { id: "groq", name: "Groq", apiBase: "https://api.groq.com/openai/v1", needsKey: true, defaultModels: ["llama-3.3-70b-versatile"] },
  { id: "ollama", name: "Ollama", apiBase: "http://localhost:11434/v1", needsKey: false, defaultModels: ["llama3.2", "mistral"] },
  { id: "openrouter", name: "OpenRouter", apiBase: "https://openrouter.ai/api/v1", needsKey: true, defaultModels: ["anthropic/claude-sonnet-4-6", "openai/gpt-4o"] },
  { id: "together", name: "Together AI", apiBase: "https://api.together.xyz/v1", needsKey: true, defaultModels: ["meta-llama/Llama-3.3-70B-Instruct-Turbo"] },
  { id: "fireworks", name: "Fireworks AI", apiBase: "https://api.fireworks.ai/inference/v1", needsKey: true, defaultModels: ["accounts/fireworks/models/llama-v3p3-70b-instruct"] },
];

const STEP_LABELS = ["Provider", "Agent", "Channel", "Launch"];

// ---------------------------------------------------------------------------
// Main Setup Wizard
// ---------------------------------------------------------------------------
export default function SetupPage() {
  const router = useRouter();
  const { data: setupStatus, isLoading: statusLoading } = useSetupStatus();
  const [step, setStep] = useState(0);

  // Step 1: Provider
  const [selectedProvider, setSelectedProvider] = useState<ProviderMeta | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [providerCreated, setProviderCreated] = useState(false);

  // Step 2: Agent
  const [agentName, setAgentName] = useState("Clawhive");
  const [agentEmoji, setAgentEmoji] = useState("\u{1F41D}");
  const [selectedModel, setSelectedModel] = useState("");
  const [agentCreated, setAgentCreated] = useState(false);

  // Step 3: Channel
  const [channelKind, setChannelKind] = useState<"telegram" | "discord" | null>(null);
  const [channelToken, setChannelToken] = useState("");
  const [channelConnectorId, setChannelConnectorId] = useState("");
  const [channelCreated, setChannelCreated] = useState(false);

  // Step 4: Launch
  const [restarting, setRestarting] = useState(false);

  const createProvider = useCreateProvider();
  const createAgent = useCreateAgent();
  const addConnector = useAddConnector();
  const restart = useRestart();

  // If already configured, redirect to dashboard
  useEffect(() => {
    if (setupStatus && !setupStatus.needs_setup) {
      router.replace("/");
    }
  }, [setupStatus, router]);

  // Set defaults when provider is selected
  useEffect(() => {
    if (selectedProvider) {
      setApiBase(selectedProvider.apiBase);
      if (selectedProvider.defaultModels.length > 0) {
        setSelectedModel(selectedProvider.defaultModels[0]);
      }
    }
  }, [selectedProvider]);

  const canAdvance = useCallback(() => {
    switch (step) {
      case 0: return providerCreated;
      case 1: return agentCreated;
      case 2: return true; // Channel is optional
      case 3: return false;
      default: return false;
    }
  }, [step, providerCreated, agentCreated]);

  const handleCreateProvider = async () => {
    if (!selectedProvider) return;
    try {
      await createProvider.mutateAsync({
        provider_id: selectedProvider.id,
        api_base: apiBase || selectedProvider.apiBase,
        api_key: selectedProvider.needsKey ? apiKey : undefined,
        models: selectedProvider.defaultModels,
      });
      setProviderCreated(true);
    } catch {
      // error is handled by mutation state
    }
  };

  const handleCreateAgent = async () => {
    if (!selectedModel) return;
    try {
      await createAgent.mutateAsync({
        agent_id: "clawhive-main",
        name: agentName || "Clawhive",
        emoji: agentEmoji || "\u{1F41D}",
        primary_model: selectedModel,
      });
      setAgentCreated(true);
    } catch {
      // error is handled by mutation state
    }
  };

  const handleAddChannel = async () => {
    if (!channelKind || !channelToken || !channelConnectorId) return;
    try {
      await addConnector.mutateAsync({
        kind: channelKind,
        connectorId: channelConnectorId,
        token: channelToken,
      });
      setChannelCreated(true);
    } catch {
      // error is handled by mutation state
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
        const res = await fetch(
          `${process.env.NEXT_PUBLIC_API_URL || "http://localhost:3001"}/api/setup/status`
        );
        if (res.ok) {
          clearInterval(poll);
          router.replace("/");
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
              providers={PROVIDERS}
              selected={selectedProvider}
              onSelect={setSelectedProvider}
              apiKey={apiKey}
              onApiKeyChange={setApiKey}
              apiBase={apiBase}
              onApiBaseChange={setApiBase}
              onSubmit={handleCreateProvider}
              isCreating={createProvider.isPending}
              isCreated={providerCreated}
              error={createProvider.error?.message}
            />
          )}
          {step === 1 && (
            <StepAgent
              name={agentName}
              onNameChange={setAgentName}
              emoji={agentEmoji}
              onEmojiChange={setAgentEmoji}
              models={selectedProvider?.defaultModels ?? []}
              selectedModel={selectedModel}
              onModelChange={setSelectedModel}
              onSubmit={handleCreateAgent}
              isCreating={createAgent.isPending}
              isCreated={agentCreated}
              error={createAgent.error?.message}
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
              onSubmit={handleAddChannel}
              isCreating={addConnector.isPending}
              isCreated={channelCreated}
              error={addConnector.error?.message}
            />
          )}
          {step === 3 && (
            <StepLaunch
              provider={selectedProvider}
              agentName={agentName}
              agentEmoji={agentEmoji}
              model={selectedModel}
              channel={channelKind}
              onLaunch={handleLaunch}
              restarting={restarting}
            />
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
          {step < 3 && (
            <Button
              size="sm"
              onClick={() => setStep((s) => s + 1)}
              disabled={!canAdvance()}
            >
              {step === 2 ? (channelCreated ? "Next" : "Skip") : "Next"}
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
  error,
}: {
  providers: ProviderMeta[];
  selected: ProviderMeta | null;
  onSelect: (p: ProviderMeta) => void;
  apiKey: string;
  onApiKeyChange: (v: string) => void;
  apiBase: string;
  onApiBaseChange: (v: string) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
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
            {selected.needsKey && (
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

            {selected.id === "ollama" && (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  API URL
                </label>
                <Input
                  placeholder="http://localhost:11434/v1"
                  value={apiBase}
                  onChange={(e) => onApiBaseChange(e.target.value)}
                  disabled={isCreated}
                  className="mt-1.5"
                />
              </div>
            )}

            <div className="flex items-center justify-between">
              <p className="text-xs text-muted-foreground">
                Models: {selected.defaultModels.join(", ")}
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
                  disabled={isCreating || (selected.needsKey && !apiKey)}
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
const EMOJI_OPTIONS = ["\u{1F41D}", "\u{1F916}", "\u{1F9E0}", "\u{26A1}", "\u{1F680}", "\u{1F4A1}", "\u{1F33F}", "\u{1F525}"];

function StepAgent({
  name,
  onNameChange,
  emoji,
  onEmojiChange,
  models,
  selectedModel,
  onModelChange,
  onSubmit,
  isCreating,
  isCreated,
  error,
}: {
  name: string;
  onNameChange: (v: string) => void;
  emoji: string;
  onEmojiChange: (v: string) => void;
  models: string[];
  selectedModel: string;
  onModelChange: (v: string) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
  error?: string;
}) {
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
          <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Model
          </label>
          <div className="mt-1.5 flex flex-wrap gap-2">
            {models.map((m) => (
              <button
                key={m}
                onClick={() => { if (!isCreated) onModelChange(m); }}
                disabled={isCreated}
                className={`rounded-md border px-3 py-1.5 text-xs font-medium transition-all ${
                  selectedModel === m
                    ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                    : "border-border hover:border-primary/40"
                } ${isCreated ? "cursor-not-allowed" : "cursor-pointer"}`}
              >
                {m}
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
function StepChannel({
  kind,
  onKindChange,
  token,
  onTokenChange,
  connectorId,
  onConnectorIdChange,
  onSubmit,
  isCreating,
  isCreated,
  error,
}: {
  kind: "telegram" | "discord" | null;
  onKindChange: (v: "telegram" | "discord") => void;
  token: string;
  onTokenChange: (v: string) => void;
  connectorId: string;
  onConnectorIdChange: (v: string) => void;
  onSubmit: () => void;
  isCreating: boolean;
  isCreated: boolean;
  error?: string;
}) {
  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-lg font-semibold">Connect a channel</h2>
        <p className="text-sm text-muted-foreground mt-1">
          Optional: connect Telegram or Discord so your agent can chat there.
          You can always set this up later from the dashboard.
        </p>
      </div>

      <div className="grid grid-cols-2 gap-3">
        {(["telegram", "discord"] as const).map((ch) => (
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
            <div className="text-sm font-medium capitalize">{ch}</div>
            <div className="mt-0.5 text-xs text-muted-foreground">
              {ch === "telegram" ? "Add a Telegram bot" : "Add a Discord bot"}
            </div>
          </button>
        ))}
      </div>

      {kind && (
        <Card className="border-primary/20 bg-primary/[0.02]">
          <CardContent className="space-y-4">
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Connector ID
              </label>
              <Input
                placeholder={kind === "telegram" ? "tg_main" : "dc_main"}
                value={connectorId}
                onChange={(e) => onConnectorIdChange(e.target.value)}
                disabled={isCreated}
                className="mt-1.5"
              />
            </div>
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Bot Token
              </label>
              <Input
                type="password"
                placeholder={kind === "telegram" ? "123456:ABC-DEF..." : "Bot token from Discord Developer Portal"}
                value={token}
                onChange={(e) => onTokenChange(e.target.value)}
                disabled={isCreated}
                className="mt-1.5"
              />
            </div>

            <div className="flex items-center justify-between">
              <a
                href={kind === "telegram" ? "https://t.me/BotFather" : "https://discord.com/developers/applications"}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1 text-xs text-primary hover:underline"
              >
                Get a bot token <ExternalLink className="h-3 w-3" />
              </a>
              {isCreated ? (
                <span className="flex items-center gap-1 text-xs font-medium text-emerald-600">
                  <CheckCircle2 className="h-3.5 w-3.5" />
                  Added
                </span>
              ) : (
                <Button
                  size="sm"
                  onClick={onSubmit}
                  disabled={isCreating || !token || !connectorId}
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
// Step 4: Launch
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
  provider: ProviderMeta | null;
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
