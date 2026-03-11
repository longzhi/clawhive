import { useState, useRef, useEffect } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Brain, Loader2, CheckCircle, Key, ShieldCheck, Plus, ChevronDown, X, Pencil, Trash2 } from "lucide-react";
import { useAuthStatus, useProviders, useTestProvider, useSetProviderKey, useCreateProvider, useProviderPresets, useUpdateProvider, useDeleteProvider } from "@/hooks/use-api";
import type { AuthStatus, ProviderPreset, ProviderSummary } from "@/hooks/use-api";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";
import { OpenAiOAuthSetup } from "@/components/providers/openai-oauth-setup";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";

function resolveAuthProfile(
  provider: ProviderSummary,
  authStatus: AuthStatus | undefined,
) {
  if (!authStatus) return undefined;
  if (provider.auth_profile) {
    return authStatus.profiles.find((profile) => profile.name === provider.auth_profile);
  }
  if (provider.provider_id === "openai-chatgpt") {
    return (
      authStatus.profiles.find((profile) => profile.kind === "OpenAiOAuth" && profile.active) ??
      authStatus.profiles.find((profile) => profile.kind === "OpenAiOAuth")
    );
  }
  return authStatus.profiles.find(
    (profile) => profile.provider === provider.provider_id && profile.active,
  );
}

function loginHint(providerId: string) {
  if (providerId === "openai" || providerId === "openai-chatgpt") {
    return "clawhive auth login openai";
  }
  return "clawhive auth login anthropic";
}

// Provider presets are fetched from the backend API (single source of truth).

// ---------------------------------------------------------------------------
// Model Multi-Select Dropdown
// ---------------------------------------------------------------------------
function ModelMultiSelect({
  defaultModels,
  selectedModels,
  customModels,
  onToggle,
  onAddCustom,
  onRemoveCustom,
}: {
  defaultModels: string[];
  selectedModels: Set<string>;
  customModels: string[];
  onToggle: (model: string) => void;
  onAddCustom: (model: string) => void;
  onRemoveCustom: (model: string) => void;
}) {
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const [customInput, setCustomInput] = useState("");
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setDropdownOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, []);

  const allModels = [...defaultModels, ...customModels];
  const selectedCount = selectedModels.size;

  return (
    <div ref={containerRef} className="relative">
      <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
        Models
      </label>
      {/* Selected tags */}
      <button
        type="button"
        onClick={() => setDropdownOpen(!dropdownOpen)}
        className="mt-1 flex w-full items-center justify-between rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background hover:bg-muted/50"
      >
        <span className="flex flex-wrap gap-1">
          {selectedCount === 0 ? (
            <span className="text-muted-foreground">Select models...</span>
          ) : (
            Array.from(selectedModels).map((model) => (
              <span
                key={model}
                className="inline-flex items-center gap-0.5 rounded bg-primary/10 text-primary px-1.5 py-0.5 text-xs font-medium"
              >
                {model}
                <X
                  className="h-3 w-3 cursor-pointer hover:text-destructive"
                  onClick={(e) => {
                    e.stopPropagation();
                    if (customModels.includes(model)) onRemoveCustom(model);
                    else onToggle(model);
                  }}
                />
              </span>
            ))
          )}
        </span>
        <ChevronDown className={`h-4 w-4 shrink-0 text-muted-foreground transition-transform ${dropdownOpen ? "rotate-180" : ""}`} />
      </button>
      {/* Dropdown */}
      {dropdownOpen && (
        <div className="absolute z-50 mt-1 w-full rounded-md border bg-popover shadow-md">
          <div className="max-h-48 overflow-y-auto p-1">
            {allModels.map((model) => (
              <label
                key={model}
                className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-sm hover:bg-muted"
              >
                <input
                  type="checkbox"
                  checked={selectedModels.has(model)}
                  onChange={() => {
                    if (customModels.includes(model) && selectedModels.has(model)) {
                      onRemoveCustom(model);
                    } else {
                      onToggle(model);
                    }
                  }}
                  className="h-3.5 w-3.5 rounded border-input accent-primary"
                />
                <span>{model}</span>
                {customModels.includes(model) && (
                  <span className="ml-auto text-[10px] text-muted-foreground">custom</span>
                )}
              </label>
            ))}
          </div>
          <div className="border-t p-2">
            <div className="flex gap-1.5">
              <Input
                placeholder="Add custom model..."
                value={customInput}
                onChange={(e) => setCustomInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    const v = customInput.trim();
                    if (v) { onAddCustom(v); setCustomInput(""); }
                  }
                }}
                className="h-7 text-xs"
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="h-7 px-2 text-xs"
                onClick={() => {
                  const v = customInput.trim();
                  if (v) { onAddCustom(v); setCustomInput(""); }
                }}
                disabled={!customInput.trim()}
              >
                Add
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Add Provider Dialog
// ---------------------------------------------------------------------------
function AddProviderDialog({ existingIds }: { existingIds: Set<string> }) {
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<ProviderPreset | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set());
  const [customModels, setCustomModels] = useState<string[]>([]);
  const createProvider = useCreateProvider();
  const { data: presets } = useProviderPresets();
  const { data: authStatus } = useAuthStatus();
  const openAiOAuthProfileName =
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth" && profile.active)?.name ??
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth")?.name ??
    "openai-oauth";
  const hasOpenAiOAuth =
    authStatus?.profiles.some((profile) => profile.kind === "OpenAiOAuth") ?? false;

  const presetModelIds = (preset: ProviderPreset) =>
    preset.models.map((model) => model.id);

  const reset = () => {
    setSelected(null);
    setApiKey("");
    setApiBase("");
    setSelectedModels(new Set());
    setCustomModels([]);
  };

  const handleSelect = (p: ProviderPreset) => {
    setSelected(p);
    setApiBase(p.api_base);
    setSelectedModels(new Set(presetModelIds(p)));
    setCustomModels([]);
  };

  const toggleModel = (model: string) => {
    setSelectedModels((prev) => {
      const next = new Set(prev);
      if (next.has(model)) next.delete(model);
      else next.add(model);
      return next;
    });
  };

  const handleSubmit = async () => {
    if (!selected) return;
    const modelList = Array.from(selectedModels);
    try {
      await createProvider.mutateAsync({
        provider_id: selected.id,
        api_base: apiBase || selected.api_base,
        api_key: selected.needs_key ? apiKey || undefined : undefined,
        auth_profile:
          selected.id === "openai-chatgpt"
            ? openAiOAuthProfileName
            : undefined,
        models: modelList.length > 0 ? modelList : presetModelIds(selected),
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
      <DialogContent className="max-w-lg max-h-[85vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Add Provider</DialogTitle>
          <DialogDescription>Select an LLM provider to configure.</DialogDescription>
        </DialogHeader>

        <div className="grid grid-cols-3 gap-2">
          {(presets ?? []).map((p) => {
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
            {selected.id === "openai-chatgpt" && (
              <OpenAiOAuthSetup />
            )}
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
            {selected.needs_key && (
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
            <ModelMultiSelect
              defaultModels={presetModelIds(selected)}
              selectedModels={selectedModels}
              customModels={customModels}
              onToggle={toggleModel}
              onAddCustom={(model) => {
                if (selectedModels.has(model) || customModels.includes(model)) return;
                setCustomModels((prev) => [...prev, model]);
                setSelectedModels((prev) => new Set([...prev, model]));
              }}
              onRemoveCustom={(model) => {
                setCustomModels((prev) => prev.filter((m) => m !== model));
                setSelectedModels((prev) => { const next = new Set(prev); next.delete(model); return next; });
              }}
            />
          </div>
        )}

        <DialogFooter>
          <Button
            onClick={handleSubmit}
            disabled={
              !selected ||
              createProvider.isPending ||
              (selected.needs_key && !apiKey) ||
              (selected.id === "openai-chatgpt" && !hasOpenAiOAuth)
            }
          >
            {createProvider.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Add Provider"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Edit Provider Dialog
// ---------------------------------------------------------------------------
function EditProviderDialog({
  provider,
  presets,
}: {
  provider: ProviderSummary;
  presets: ProviderPreset[];
}) {
  const [open, setOpen] = useState(false);
  const [apiBase, setApiBase] = useState(provider.api_base);
  const preset = presets.find((p) => p.id === provider.provider_id);
  const defaultModels = preset?.models.map((model) => model.id) ?? [];
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set(provider.models));
  const [customModels, setCustomModels] = useState<string[]>(
    provider.models.filter((m) => !defaultModels.includes(m))
  );
  const updateProvider = useUpdateProvider();

  const reset = () => {
    setApiBase(provider.api_base);
    setSelectedModels(new Set(provider.models));
    setCustomModels(provider.models.filter((m) => !defaultModels.includes(m)));
  };

  const toggleModel = (model: string) => {
    setSelectedModels((prev) => {
      const next = new Set(prev);
      if (next.has(model)) next.delete(model);
      else next.add(model);
      return next;
    });
  };

  const handleSave = async () => {
    try {
      await updateProvider.mutateAsync({
        id: provider.provider_id,
        data: {
          api_base: apiBase,
          models: Array.from(selectedModels),
        },
      });
      toast.success(`Provider ${provider.provider_id} updated`);
      setOpen(false);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "Unknown error";
      toast.error(`Failed to update provider: ${msg}`);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); if (!v) reset(); }}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="flex-1 gap-1.5">
          <Pencil className="h-4 w-4" />
          Edit
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Edit Provider: {provider.provider_id}</DialogTitle>
          <DialogDescription>Update provider configuration.</DialogDescription>
        </DialogHeader>
        <div className="space-y-3 rounded-lg border p-4">
          <div>
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Provider ID
            </label>
            <div className="mt-1">
              <Badge variant="outline" className="font-mono">{provider.provider_id}</Badge>
            </div>
          </div>
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
          <ModelMultiSelect
            defaultModels={defaultModels}
            selectedModels={selectedModels}
            customModels={customModels}
            onToggle={toggleModel}
            onAddCustom={(model) => {
              if (selectedModels.has(model) || customModels.includes(model)) return;
              setCustomModels((prev) => [...prev, model]);
              setSelectedModels((prev) => new Set([...prev, model]));
            }}
            onRemoveCustom={(model) => {
              setCustomModels((prev) => prev.filter((m) => m !== model));
              setSelectedModels((prev) => { const next = new Set(prev); next.delete(model); return next; });
            }}
          />
        </div>
        <DialogFooter>
          <Button onClick={handleSave} disabled={updateProvider.isPending}>
            {updateProvider.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Save Changes"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Providers Skeleton
// ---------------------------------------------------------------------------
function ProvidersSkeleton() {
  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <Skeleton className="h-5 w-24" />
          <Skeleton className="h-4 w-48 mt-1" />
        </div>
        <Skeleton className="h-9 w-32" />
      </div>
      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {Array.from({ length: 3 }).map((_, i) => (
          <Card key={i}>
            <CardHeader>
              <Skeleton className="h-5 w-28" />
              <Skeleton className="h-3 w-40 mt-1" />
            </CardHeader>
            <CardContent className="grid gap-4 pt-4">
              <Skeleton className="h-4 w-full" />
              <Skeleton className="h-4 w-3/4" />
              <Skeleton className="h-9 w-full" />
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}

export default function ProvidersPage() {
  const { data: providers, isLoading, isError, error, refetch } = useProviders();
  const { data: authStatus } = useAuthStatus();
  const { data: presets } = useProviderPresets();
  const testProvider = useTestProvider();
  const setProviderKey = useSetProviderKey();
  const deleteProvider = useDeleteProvider();
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [oauthLoginProviderId, setOauthLoginProviderId] = useState<string | null>(null);

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

  const handleShowLoginHint = (providerId: string) => {
    if (providerId === "openai" || providerId === "openai-chatgpt") {
      setOauthLoginProviderId(providerId);
      return;
    }
    toast.message(`Use CLI: ${loginHint(providerId)}`);
  };

  const handleDelete = async () => {
    if (!deleteTarget) return;
    try {
      await deleteProvider.mutateAsync(deleteTarget);
      toast.success(`Provider ${deleteTarget} deleted`);
    } catch (e) {
      toast.error("Failed to delete provider");
    } finally {
      setDeleteTarget(null);
    }
  };

  if (isLoading) return <ProvidersSkeleton />;
  if (isError) return <ErrorState message={error?.message} onRetry={refetch} />;

  return (
    <><div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Providers</h2>
          <p className="text-sm text-muted-foreground">Manage your LLM provider connections.</p>
        </div>
        <AddProviderDialog existingIds={existingIds} />
      </div>

      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        {providers?.map((provider) => {
          const authProfile = resolveAuthProfile(provider, authStatus);
          const usesOAuth =
            Boolean(provider.auth_profile) ||
            provider.provider_id === "openai-chatgpt";

          return (
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
                    className={
                      usesOAuth
                        ? ""
                        : provider.key_configured
                          ? "bg-green-500 hover:bg-green-600"
                          : "bg-amber-500 hover:bg-amber-600 text-white"
                    }
                  >
                    {usesOAuth ? "Not Used" : provider.key_configured ? "Configured" : "Not Set"}
                  </Badge>
                </div>

                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm text-muted-foreground">OAuth / Session</span>
                  {authProfile ? (
                    <Badge className="bg-emerald-600 hover:bg-emerald-700">
                      <ShieldCheck className="mr-1 h-3.5 w-3.5" />
                      {provider.auth_profile ?? authProfile.name}
                    </Badge>
                  ) : usesOAuth ? (
                    <Button
                      variant="secondary"
                      size="sm"
                      className="h-7"
                      onClick={() => handleShowLoginHint(provider.provider_id)}
                    >
                      Login
                    </Button>
                  ) : (
                    <Badge variant="secondary">Optional</Badge>
                  )}
                </div>

                {!usesOAuth && (
                  <div className="flex flex-col gap-1">
                    <div className="flex items-center gap-2">
                      <div className="relative flex-1">
                        <Key className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                        <Input
                          type="password"
                          placeholder="Enter API key..."
                          className="pl-9 h-9 text-sm"
                          value={keys[provider.provider_id] || ""}
                          onChange={(e) => setKeys((prev) => ({ ...prev, [provider.provider_id]: e.target.value }))}
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
                  </div>
                )}

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
                <div className="flex gap-2 mt-2">
                  <EditProviderDialog provider={provider} presets={presets ?? []} />
                  <Button
                    variant="destructive"
                    size="sm"
                    className="flex-1 gap-1.5"
                    onClick={() => setDeleteTarget(provider.provider_id)}
                  >
                    <Trash2 className="h-4 w-4" />
                    Delete
                  </Button>
                </div>
              </CardContent>
            </Card>
          );
        })}

        {providers?.length === 0 && (
          <div className="col-span-full text-center text-muted-foreground p-8">
            No providers configured
          </div>
        )}
      </div>
    </div>
      <ConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(open) => { if (!open) setDeleteTarget(null); }}
        title={`Delete provider '${deleteTarget ?? ""}'?`}
        description="Agents using this provider's models will lose access."
        confirmLabel="Delete"
        variant="destructive"
        onConfirm={handleDelete}
        loading={deleteProvider.isPending}
      />
      <Dialog
        open={oauthLoginProviderId !== null}
        onOpenChange={(open) => {
          if (!open) setOauthLoginProviderId(null);
        }}
      >
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>Connect ChatGPT OAuth</DialogTitle>
            <DialogDescription>
              Complete the OpenAI OAuth flow in the browser, then return here to test the provider.
            </DialogDescription>
          </DialogHeader>
          <OpenAiOAuthSetup />
        </DialogContent>
      </Dialog>
  </>  );
}
