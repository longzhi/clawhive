import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Brain, Loader2, CheckCircle, Key, ShieldCheck, Plus } from "lucide-react";
import { useAuthStatus, useProviders, useTestProvider, useSetProviderKey, useCreateProvider } from "@/hooks/use-api";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";

// ---------------------------------------------------------------------------
// Known provider presets (mirrors Setup.tsx)
// ---------------------------------------------------------------------------
interface ProviderMeta {
  id: string;
  name: string;
  apiBase: string;
  needsKey: boolean;
  defaultModels: string[];
}

const KNOWN_PROVIDERS: ProviderMeta[] = [
  { id: "anthropic", name: "Anthropic", apiBase: "https://api.anthropic.com/v1", needsKey: true, defaultModels: ["claude-sonnet-4-6", "claude-haiku-4-5"] },
  { id: "openai", name: "OpenAI", apiBase: "https://api.openai.com/v1", needsKey: true, defaultModels: ["gpt-4o", "gpt-4o-mini"] },
  { id: "azure-openai", name: "Azure OpenAI", apiBase: "https://myresource.openai.azure.com/openai/v1", needsKey: true, defaultModels: ["gpt-4o", "gpt-4o-mini"] },
  { id: "gemini", name: "Google Gemini", apiBase: "https://generativelanguage.googleapis.com/v1beta", needsKey: true, defaultModels: ["gemini-2.5-pro", "gemini-2.5-flash"] },
  { id: "deepseek", name: "DeepSeek", apiBase: "https://api.deepseek.com/v1", needsKey: true, defaultModels: ["deepseek-chat", "deepseek-reasoner"] },
  { id: "groq", name: "Groq", apiBase: "https://api.groq.com/openai/v1", needsKey: true, defaultModels: ["llama-3.3-70b-versatile"] },
  { id: "ollama", name: "Ollama", apiBase: "http://localhost:11434/v1", needsKey: false, defaultModels: ["llama3.2", "mistral"] },
  { id: "openrouter", name: "OpenRouter", apiBase: "https://openrouter.ai/api/v1", needsKey: true, defaultModels: ["anthropic/claude-sonnet-4-6", "openai/gpt-4o"] },
  { id: "together", name: "Together AI", apiBase: "https://api.together.xyz/v1", needsKey: true, defaultModels: ["meta-llama/Llama-3.3-70B-Instruct-Turbo"] },
  { id: "fireworks", name: "Fireworks AI", apiBase: "https://api.fireworks.ai/inference/v1", needsKey: true, defaultModels: ["accounts/fireworks/models/llama-v3p3-70b-instruct"] },
];

// ---------------------------------------------------------------------------
// Add Provider Dialog
// ---------------------------------------------------------------------------
function AddProviderDialog({ existingIds }: { existingIds: Set<string> }) {
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<ProviderMeta | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set());
  const [customModels, setCustomModels] = useState<string[]>([]);
  const [customInput, setCustomInput] = useState("");
  const createProvider = useCreateProvider();

  const reset = () => {
    setSelected(null);
    setApiKey("");
    setApiBase("");
    setSelectedModels(new Set());
    setCustomModels([]);
    setCustomInput("");
  };

  const handleSelect = (p: ProviderMeta) => {
    setSelected(p);
    setApiBase(p.apiBase);
    setSelectedModels(new Set(p.defaultModels));
    setCustomModels([]);
    setCustomInput("");
  };

  const toggleModel = (model: string) => {
    setSelectedModels((prev) => {
      const next = new Set(prev);
      if (next.has(model)) next.delete(model);
      else next.add(model);
      return next;
    });
  };

  const addCustomModel = () => {
    const model = customInput.trim();
    if (!model || selectedModels.has(model) || customModels.includes(model)) return;
    setCustomModels((prev) => [...prev, model]);
    setSelectedModels((prev) => new Set([...prev, model]));
    setCustomInput("");
  };

  const handleSubmit = async () => {
    if (!selected) return;
    const modelList = Array.from(selectedModels);
    try {
      await createProvider.mutateAsync({
        provider_id: selected.id,
        api_base: apiBase || selected.apiBase,
        api_key: selected.needsKey ? apiKey || undefined : undefined,
        models: modelList.length > 0 ? modelList : selected.defaultModels,
      });
      toast.success(`Provider ${selected.name} added`);
      reset();
      setOpen(false);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "Unknown error";
      if (msg.includes("409") || msg.includes("already exists") || msg.includes("Conflict")) {
        toast.error("Provider already exists");
      } else {
        toast.error(`Failed to add provider: ${msg}`);
      }
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); if (!v) reset(); }}>
      <DialogTrigger asChild>
        <Button size="sm" className="gap-1.5">
          <Plus className="h-4 w-4" />
          Add Provider
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Add Provider</DialogTitle>
          <DialogDescription>Select an LLM provider to configure.</DialogDescription>
        </DialogHeader>

        <div className="grid grid-cols-3 gap-2">
          {KNOWN_PROVIDERS.map((p) => {
            const exists = existingIds.has(p.id);
            return (
              <button
                key={p.id}
                onClick={() => !exists && handleSelect(p)}
                disabled={exists}
                className={`rounded-lg border px-3 py-2.5 text-left text-sm font-medium transition-all ${
                  selected?.id === p.id
                    ? "border-primary bg-primary/5 text-primary ring-1 ring-primary/20"
                    : exists
                      ? "border-border opacity-40 cursor-not-allowed"
                      : "border-border hover:border-primary/40 hover:bg-muted/50 cursor-pointer"
                }`}
              >
                {p.name}
                {exists && <span className="block text-[10px] text-muted-foreground">configured</span>}
              </button>
            );
          })}
        </div>

        {selected && (
          <div className="space-y-3 rounded-lg border p-4">
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                API Base
              </label>
              <Input
                value={apiBase}
                onChange={(e) => setApiBase(e.target.value)}
                className="mt-1"
              />
            </div>
            {selected.needsKey && (
              <div>
                <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                  API Key
                </label>
                <Input
                  type="password"
                  placeholder={`Enter your ${selected.name} API key`}
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  className="mt-1"
                />
              </div>
            )}
            <div>
              <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                Models
              </label>
              <div className="mt-1.5 flex flex-wrap gap-1.5">
                {selected.defaultModels.map((model) => (
                  <button
                    key={model}
                    type="button"
                    onClick={() => toggleModel(model)}
                    className={`rounded-md border px-2.5 py-1 text-xs font-medium transition-all ${
                      selectedModels.has(model)
                        ? "border-primary bg-primary/10 text-primary"
                        : "border-border text-muted-foreground hover:border-primary/40"
                    }`}
                  >
                    {model}
                  </button>
                ))}
                {customModels.map((model) => (
                  <button
                    key={model}
                    type="button"
                    onClick={() => {
                      setCustomModels((prev) => prev.filter((m) => m !== model));
                      setSelectedModels((prev) => { const next = new Set(prev); next.delete(model); return next; });
                    }}
                    className="rounded-md border border-primary bg-primary/10 text-primary px-2.5 py-1 text-xs font-medium transition-all hover:bg-destructive/10 hover:text-destructive hover:border-destructive"
                    title="Click to remove"
                  >
                    {model} &times;
                  </button>
                ))}
              </div>
              <div className="mt-2 flex gap-1.5">
                <Input
                  placeholder="Add custom model..."
                  value={customInput}
                  onChange={(e) => setCustomInput(e.target.value)}
                  onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); addCustomModel(); } }}
                  className="h-8 text-xs"
                />
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="h-8 px-3 text-xs"
                  onClick={addCustomModel}
                  disabled={!customInput.trim()}
                >
                  Add
                </Button>
              </div>
            </div>
          </div>
        )}

        <DialogFooter>
          <Button
            onClick={handleSubmit}
            disabled={!selected || createProvider.isPending || (selected.needsKey && !apiKey)}
          >
            {createProvider.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Add Provider"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
export default function ProvidersPage() {
  const { data: providers, isLoading } = useProviders();
  const { data: authStatus } = useAuthStatus();
  const testProvider = useTestProvider();
  const setProviderKey = useSetProviderKey();
  const [keys, setKeys] = useState<Record<string, string>>({});

  const existingIds = new Set(providers?.map((p) => p.provider_id) ?? []);

  const handleSaveKey = async (id: string) => {
    const apiKey = keys[id];
    if (!apiKey) return;

    try {
      await setProviderKey.mutateAsync({ id, apiKey });
      toast.success("API key saved");
      setKeys(prev => ({ ...prev, [id]: "" }));
    } catch (e) {
      toast.error("Failed to save API key");
    }
  };

  const handleTest = async (id: string) => {
    try {
      const result = await testProvider.mutateAsync(id);
      if (result.ok) {
        toast.success(`Provider ${id} is working correctly`);
      } else {
        toast.error(`Provider ${id} failed: ${result.message}`);
      }
    } catch (e) {
      toast.error(`Failed to test provider ${id}`);
    }
  };

  const authProfileForProvider = (providerId: string) =>
    authStatus?.profiles.find((p) => p.provider === providerId && p.active);

  const loginHint = (providerId: string) =>
    providerId === "openai" ? "clawhive auth login openai" : "clawhive auth login anthropic";

  const handleShowLoginHint = (providerId: string) => {
    toast.message(`Use CLI: ${loginHint(providerId)}`);
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
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Providers</h2>
          <p className="text-sm text-muted-foreground">Manage your LLM provider connections.</p>
        </div>
        <AddProviderDialog existingIds={existingIds} />
      </div>

      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {providers?.map((provider) => (
          <Card key={provider.provider_id}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <div className="flex flex-col space-y-1">
                <CardTitle className="capitalize">{provider.provider_id}</CardTitle>
                <CardDescription className="font-mono text-xs truncate max-w-[200px]">
                  {provider.api_base}
                </CardDescription>
              </div>
              <Brain className="h-6 w-6 text-muted-foreground" />
            </CardHeader>
            <CardContent className="grid gap-4 pt-4">
              <div className="flex items-center justify-between">
                <span className="text-sm text-muted-foreground">API Key</span>
                <Badge
                  variant={provider.key_configured ? "default" : "secondary"}
                  className={provider.key_configured ? "bg-green-500 hover:bg-green-600" : "bg-amber-500 hover:bg-amber-600 text-white"}
                >
                  {provider.key_configured ? "Configured" : "Not Set"}
                </Badge>
              </div>

              <div className="flex items-center justify-between">
                <span className="text-sm text-muted-foreground">OAuth / Session</span>
                {authProfileForProvider(provider.provider_id) ? (
                  <Badge className="bg-emerald-600 hover:bg-emerald-700">
                    <ShieldCheck className="mr-1 h-3.5 w-3.5" />
                    Connected
                  </Badge>
                ) : (
                  <Button
                    variant="secondary"
                    size="sm"
                    className="h-7"
                    onClick={() => handleShowLoginHint(provider.provider_id)}
                  >
                    Login
                  </Button>
                )}
              </div>

              <div className="flex flex-col gap-1">
                <div className="flex items-center gap-2">
                  <div className="relative flex-1">
                    <Key className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                    <Input
                      type="password"
                      placeholder="Enter API key..."
                      className="pl-9 h-9 text-sm"
                      value={keys[provider.provider_id] || ""}
                      onChange={(e) => setKeys(prev => ({ ...prev, [provider.provider_id]: e.target.value }))}
                    />
                  </div>
                  <Button
                    size="sm"
                    className="h-9"
                    onClick={() => handleSaveKey(provider.provider_id)}
                    disabled={setProviderKey.isPending || !keys[provider.provider_id]}
                  >
                    Save
                  </Button>
                </div>
                {provider.api_key_env && (
                  <span className="text-xs text-muted-foreground">Sets {provider.api_key_env}</span>
                )}
              </div>

              <div className="flex flex-col gap-2">
                <span className="text-sm text-muted-foreground">Models</span>
                <div className="flex flex-wrap gap-1">
                  {provider.models.map((model) => (
                    <Badge key={model} variant="outline" className="text-[10px] px-1">
                      {model}
                    </Badge>
                  ))}
                </div>
              </div>

              <Button
                variant="outline"
                size="sm"
                className="w-full mt-2"
                onClick={() => handleTest(provider.provider_id)}
                disabled={testProvider.isPending}
              >
                {testProvider.isPending ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <CheckCircle className="mr-2 h-4 w-4" />
                )}
                Test Connection
              </Button>
            </CardContent>
          </Card>
        ))}

        {providers?.length === 0 && (
          <div className="col-span-full text-center text-muted-foreground p-8">
            No providers configured
          </div>
        )}
      </div>
    </div>
  );
}
