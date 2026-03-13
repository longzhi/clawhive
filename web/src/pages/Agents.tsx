import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Plus, Loader2, X, Save } from "lucide-react";
import { useAgents, useToggleAgent, useCreateAgent, useProviders, useAgent, useUpdateAgent, type AgentDetail } from "@/hooks/use-api";
import { Switch } from "@/components/ui/switch";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";

const EMOJI_OPTIONS = ["🐝", "🤖", "🧠", "⚡", "🚀", "💡", "🌿", "🔥"];

// ---------------------------------------------------------------------------
// New Agent Dialog
// ---------------------------------------------------------------------------
function NewAgentDialog() {
  const [open, setOpen] = useState(false);
  const [agentId, setAgentId] = useState("");
  const [name, setName] = useState("");
  const [emoji, setEmoji] = useState("🤖");
  const [selectedModel, setSelectedModel] = useState("");
  const createAgent = useCreateAgent();
  const { data: providers } = useProviders();

  // Collect all models from configured providers
  const allModels = providers?.flatMap((p) => p.models.map((m) => `${p.provider_id}/${m}`)) ?? [];

  const reset = () => {
    setAgentId("");
    setName("");
    setEmoji("🤖");
    setSelectedModel("");
  };

  const handleSubmit = async () => {
    if (!agentId || !name || !selectedModel) return;
    try {
      await createAgent.mutateAsync({
        agent_id: agentId,
        name,
        emoji,
        primary_model: selectedModel,
      });
      toast.success(`Agent "${name}" created`);
      reset();
      setOpen(false);
    } catch {
      toast.error("Failed to create agent");
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); if (!v) reset(); }}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="mr-2 h-4 w-4" /> New Agent
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>New Agent</DialogTitle>
          <DialogDescription>Create a new AI agent.</DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div>
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Agent ID
            </label>
            <Input
              placeholder="my-agent"
              value={agentId}
              onChange={(e) => setAgentId(e.target.value)}
              className="mt-1"
            />
          </div>
          <div>
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Name
            </label>
            <Input
              placeholder="My Agent"
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="mt-1"
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
                  type="button"
                  onClick={() => setEmoji(e)}
                  className={`flex h-9 w-9 items-center justify-center rounded-md text-lg transition-all ${
                    emoji === e
                      ? "bg-primary/10 ring-1 ring-primary/30 scale-110"
                      : "hover:bg-muted"
                  }`}
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
            {allModels.length > 0 ? (
              <div className="mt-1.5 flex flex-wrap gap-1.5">
                {allModels.map((m) => (
                  <button
                    key={m}
                    type="button"
                    onClick={() => setSelectedModel(m)}
                    className={`rounded-md border px-2.5 py-1 text-xs font-medium transition-all ${
                      selectedModel === m
                        ? "border-primary bg-primary/10 text-primary"
                        : "border-border text-muted-foreground hover:border-primary/40"
                    }`}
                  >
                    {m}
                  </button>
                ))}
              </div>
            ) : (
              <p className="mt-1 text-xs text-muted-foreground">
                No models available. Add a provider first.
              </p>
            )}
          </div>
        </div>

        <DialogFooter>
          <Button
            onClick={handleSubmit}
            disabled={!agentId || !name || !selectedModel || createAgent.isPending}
          >
            {createAgent.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create Agent"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Agent Detail Dialog
// ---------------------------------------------------------------------------
function AgentDetailDialog({
  agentId,
  open,
  onOpenChange,
}: {
  agentId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { data: detail, isLoading } = useAgent(agentId);
  const updateAgent = useUpdateAgent();
  const { data: providers } = useProviders();
  const allModels = providers?.flatMap((p) => p.models.map((m) => `${p.provider_id}/${m}`)) ?? [];

  const [isEditing, setIsEditing] = useState(false);
  const [editData, setEditData] = useState<AgentDetail | null>(null);
  const [confirmDisable, setConfirmDisable] = useState(false);

  // tool/fallback add inputs
  const [newTool, setNewTool] = useState("");
  const [newFallback, setNewFallback] = useState("");

  const current = isEditing && editData ? editData : detail;

  const startEditing = () => {
    if (!detail) return;
    setEditData(JSON.parse(JSON.stringify(detail)));
    setIsEditing(true);
  };

  const cancelEditing = () => {
    setIsEditing(false);
    setEditData(null);
    setNewTool("");
    setNewFallback("");
  };

  const saveChanges = async () => {
    if (!editData) return;
    try {
      await updateAgent.mutateAsync({ id: agentId, data: editData });
      toast.success("Agent updated");
      setIsEditing(false);
      setEditData(null);
      setNewTool("");
      setNewFallback("");
    } catch {
      toast.error("Failed to update agent");
    }
  };

  const handleToggleEnabled = (checked: boolean) => {
    if (!editData) return;
    if (!checked) {
      setConfirmDisable(true);
    } else {
      setEditData({ ...editData, enabled: true });
    }
  };

  const confirmDisableAgent = () => {
    if (!editData) return;
    setEditData({ ...editData, enabled: false });
    setConfirmDisable(false);
  };

  const addTool = () => {
    if (!editData || !newTool.trim()) return;
    const trimmed = newTool.trim();
    if (editData.tool_policy.allow.includes(trimmed)) return;
    setEditData({ ...editData, tool_policy: { allow: [...editData.tool_policy.allow, trimmed] } });
    setNewTool("");
  };

  const removeTool = (tool: string) => {
    if (!editData) return;
    setEditData({ ...editData, tool_policy: { allow: editData.tool_policy.allow.filter((t) => t !== tool) } });
  };

  const addFallback = () => {
    if (!editData || !newFallback.trim()) return;
    const trimmed = newFallback.trim();
    if (editData.model_policy.fallbacks.includes(trimmed)) return;
    setEditData({ ...editData, model_policy: { ...editData.model_policy, fallbacks: [...editData.model_policy.fallbacks, trimmed] } });
    setNewFallback("");
  };

  const removeFallback = (model: string) => {
    if (!editData) return;
    setEditData({ ...editData, model_policy: { ...editData.model_policy, fallbacks: editData.model_policy.fallbacks.filter((m) => m !== model) } });
  };

  return (
    <>
      <Dialog open={open} onOpenChange={(v) => { if (!v) { cancelEditing(); } onOpenChange(v); }}>
        <DialogContent className="sm:max-w-2xl max-h-[85vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              {isLoading ? (
                <Skeleton className="h-6 w-40" />
              ) : (
                <>
                  <span className="text-2xl">{current?.identity.emoji}</span>
                  <span>{current?.identity.name ?? agentId}</span>
                </>
              )}
            </DialogTitle>
            <DialogDescription className="font-mono text-xs">{agentId}</DialogDescription>
          </DialogHeader>

          {isLoading ? (
            <div className="space-y-3 py-4">
              <Skeleton className="h-4 w-full" />
              <Skeleton className="h-4 w-3/4" />
              <Skeleton className="h-4 w-1/2" />
            </div>
          ) : current ? (
            <Tabs defaultValue="overview" className="w-full">
              <TabsList className="w-full">
                <TabsTrigger value="overview" className="flex-1">Overview</TabsTrigger>
                <TabsTrigger value="model" className="flex-1">Model</TabsTrigger>
                <TabsTrigger value="tools" className="flex-1">Tools</TabsTrigger>
                <TabsTrigger value="memory" className="flex-1">Memory</TabsTrigger>
                {current.sub_agent && (
                  <TabsTrigger value="subagent" className="flex-1">Sub-Agent</TabsTrigger>
                )}
              </TabsList>

              {/* ── Overview ── */}
              <TabsContent value="overview" className="space-y-4 pt-4">
                <div className="space-y-1">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Agent ID</Label>
                  <p className="font-mono text-sm bg-muted px-3 py-2 rounded-md">{current.agent_id}</p>
                </div>

                <div className="flex items-center justify-between rounded-md border px-3 py-2.5">
                  <div>
                    <p className="text-sm font-medium">Enabled</p>
                    <p className="text-xs text-muted-foreground">Agent is active and accepting requests</p>
                  </div>
                  {isEditing ? (
                    <Switch
                      checked={editData?.enabled ?? false}
                      onCheckedChange={handleToggleEnabled}
                    />
                  ) : (
                    <Badge variant={current.enabled ? "default" : "outline"} className={current.enabled ? "bg-green-500 hover:bg-green-600" : ""}>
                      {current.enabled ? "Active" : "Disabled"}
                    </Badge>
                  )}
                </div>

                <div className="space-y-1">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Name</Label>
                  {isEditing ? (
                    <Input
                      value={editData?.identity.name ?? ""}
                      onChange={(e) => editData && setEditData({ ...editData, identity: { ...editData.identity, name: e.target.value } })}
                    />
                  ) : (
                    <p className="text-sm">{current.identity.name}</p>
                  )}
                </div>

                <div className="space-y-1">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Emoji</Label>
                  {isEditing ? (
                    <div className="flex gap-1.5 flex-wrap">
                      {EMOJI_OPTIONS.map((e) => (
                        <button
                          key={e}
                          type="button"
                          onClick={() => editData && setEditData({ ...editData, identity: { ...editData.identity, emoji: e } })}
                          className={`flex h-9 w-9 items-center justify-center rounded-md text-lg transition-all ${
                            editData?.identity.emoji === e
                              ? "bg-primary/10 ring-1 ring-primary/30 scale-110"
                              : "hover:bg-muted"
                          }`}
                        >
                          {e}
                        </button>
                      ))}
                    </div>
                  ) : (
                    <p className="text-2xl">{current.identity.emoji}</p>
                  )}
                </div>
              </TabsContent>

              {/* ── Model ── */}
              <TabsContent value="model" className="space-y-4 pt-4">
                <div className="space-y-2">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Primary Model</Label>
                  {isEditing ? (
                    allModels.length > 0 ? (
                      <div className="flex flex-wrap gap-1.5">
                        {allModels.map((m) => (
                          <button
                            key={m}
                            type="button"
                            onClick={() => editData && setEditData({ ...editData, model_policy: { ...editData.model_policy, primary: m } })}
                            className={`rounded-md border px-2.5 py-1 text-xs font-medium transition-all ${
                              editData?.model_policy.primary === m
                                ? "border-primary bg-primary/10 text-primary"
                                : "border-border text-muted-foreground hover:border-primary/40"
                            }`}
                          >
                            {m}
                          </button>
                        ))}
                      </div>
                    ) : (
                      <Input
                        value={editData?.model_policy.primary ?? ""}
                        onChange={(e) => editData && setEditData({ ...editData, model_policy: { ...editData.model_policy, primary: e.target.value } })}
                        placeholder="e.g. claude-3-5-sonnet-20241022"
                      />
                    )
                  ) : (
                    <p className="font-mono text-sm bg-muted px-3 py-2 rounded-md">{current.model_policy.primary}</p>
                  )}
                </div>

                <div className="space-y-2">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Fallback Models</Label>
                  <div className="flex flex-wrap gap-1.5 min-h-[2rem]">
                    {current.model_policy.fallbacks.length === 0 && !isEditing && (
                      <span className="text-xs text-muted-foreground">No fallbacks configured</span>
                    )}
                    {current.model_policy.fallbacks.map((m) => (
                      <Badge key={m} variant="secondary" className="gap-1 pl-2 pr-1 py-0.5">
                        <span className="font-mono text-xs">{m}</span>
                        {isEditing && (
                          <button
                            type="button"
                            onClick={() => removeFallback(m)}
                            className="ml-0.5 rounded-sm hover:bg-muted-foreground/20"
                          >
                            <X className="h-3 w-3" />
                          </button>
                        )}
                      </Badge>
                    ))}
                  </div>
                  {isEditing && (
                    <div className="flex gap-2">
                      <Input
                        className="h-8 text-xs"
                        placeholder="model name"
                        value={newFallback}
                        onChange={(e) => setNewFallback(e.target.value)}
                        onKeyDown={(e) => e.key === "Enter" && addFallback()}
                      />
                      <Button size="sm" variant="outline" onClick={addFallback} disabled={!newFallback.trim()}>
                        <Plus className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  )}
                </div>
              </TabsContent>

              {/* ── Tools ── */}
              <TabsContent value="tools" className="space-y-4 pt-4">
                <div className="space-y-2">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Allowed Tools</Label>
                  <div className="flex flex-wrap gap-1.5 min-h-[2rem]">
                    {current.tool_policy.allow.length === 0 && !isEditing && (
                      <span className="text-xs text-muted-foreground">No tools configured</span>
                    )}
                    {current.tool_policy.allow.map((tool) => (
                      <Badge key={tool} variant="secondary" className="gap-1 pl-2 pr-1 py-0.5">
                        <span className="text-xs">{tool}</span>
                        {isEditing && (
                          <button
                            type="button"
                            onClick={() => removeTool(tool)}
                            className="ml-0.5 rounded-sm hover:bg-muted-foreground/20"
                          >
                            <X className="h-3 w-3" />
                          </button>
                        )}
                      </Badge>
                    ))}
                  </div>
                  {isEditing && (
                    <div className="flex gap-2">
                      <Input
                        className="h-8 text-xs"
                        placeholder="tool name"
                        value={newTool}
                        onChange={(e) => setNewTool(e.target.value)}
                        onKeyDown={(e) => e.key === "Enter" && addTool()}
                      />
                      <Button size="sm" variant="outline" onClick={addTool} disabled={!newTool.trim()}>
                        <Plus className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  )}
                </div>
              </TabsContent>

              {/* ── Memory ── */}
              <TabsContent value="memory" className="space-y-4 pt-4">
                <div className="space-y-1">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Memory Mode</Label>
                  {isEditing ? (
                    <Select
                      value={editData?.memory_policy.mode ?? "standard"}
                      onValueChange={(v) => editData && setEditData({ ...editData, memory_policy: { ...editData.memory_policy, mode: v } })}
                    >
                      <SelectTrigger className="h-9">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="standard">standard</SelectItem>
                        <SelectItem value="none">none</SelectItem>
                      </SelectContent>
                    </Select>
                  ) : (
                    <p className="text-sm font-mono bg-muted px-3 py-2 rounded-md">{current.memory_policy.mode}</p>
                  )}
                </div>

                <div className="space-y-1">
                  <Label className="text-xs text-muted-foreground uppercase tracking-wide">Write Scope</Label>
                  {isEditing ? (
                    <Select
                      value={editData?.memory_policy.write_scope ?? "all"}
                      onValueChange={(v) => editData && setEditData({ ...editData, memory_policy: { ...editData.memory_policy, write_scope: v } })}
                    >
                      <SelectTrigger className="h-9">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="all">all</SelectItem>
                        <SelectItem value="none">none</SelectItem>
                      </SelectContent>
                    </Select>
                  ) : (
                    <p className="text-sm font-mono bg-muted px-3 py-2 rounded-md">{current.memory_policy.write_scope}</p>
                  )}
                </div>
              </TabsContent>

              {/* ── Sub-Agent ── */}
              {current.sub_agent && (
                <TabsContent value="subagent" className="space-y-4 pt-4">
                  <div className="flex items-center justify-between rounded-md border px-3 py-2.5">
                    <div>
                      <p className="text-sm font-medium">Allow Spawn</p>
                      <p className="text-xs text-muted-foreground">Allow this agent to spawn sub-agents</p>
                    </div>
                    {isEditing ? (
                      <Switch
                        checked={editData?.sub_agent?.allow_spawn ?? false}
                        onCheckedChange={(checked) =>
                          editData && setEditData({ ...editData, sub_agent: { allow_spawn: checked } })
                        }
                      />
                    ) : (
                      <Badge variant={current.sub_agent.allow_spawn ? "default" : "outline"}>
                        {current.sub_agent.allow_spawn ? "Enabled" : "Disabled"}
                      </Badge>
                    )}
                  </div>
                </TabsContent>
              )}
            </Tabs>
          ) : (
            <div className="text-center text-muted-foreground py-8">Agent not found</div>
          )}

          <DialogFooter>
            {isEditing ? (
              <>
                <Button variant="outline" onClick={cancelEditing}>Cancel</Button>
                <Button onClick={saveChanges} disabled={updateAgent.isPending}>
                  {updateAgent.isPending && <Loader2 className="h-4 w-4 mr-2 animate-spin" />}
                  <Save className="h-4 w-4 mr-1" />
                  Save
                </Button>
              </>
            ) : (
              <>
                <Button variant="outline" onClick={() => onOpenChange(false)}>Close</Button>
                <Button variant="secondary" onClick={startEditing} disabled={!detail}>
                  Edit
                </Button>
              </>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={confirmDisable}
        onOpenChange={setConfirmDisable}
        title="Disable Agent"
        description="Disabling this agent will stop it from accepting requests. Are you sure?"
        confirmLabel="Disable"
        variant="destructive"
        onConfirm={confirmDisableAgent}
      />
    </>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
export default function AgentsPage() {
  const { data: agents, isLoading, isError, error, refetch } = useAgents();
  const toggleAgent = useToggleAgent();
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);

  if (isError) return <ErrorState message={error?.message} onRetry={refetch} />;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-2xl font-bold tracking-tight">Agents</h2>
        <NewAgentDialog />
      </div>

      <div className="rounded-md border bg-card">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Agent</TableHead>
              <TableHead>Model</TableHead>
              <TableHead>Tools</TableHead>
              <TableHead>Status</TableHead>
              <TableHead className="w-[100px]">Enabled</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {isLoading ? (
              Array.from({ length: 3 }).map((_, i) => (
                <TableRow key={i}>
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <Skeleton className="h-8 w-8 rounded-full" />
                      <div className="flex flex-col gap-1">
                        <Skeleton className="h-4 w-24" />
                        <Skeleton className="h-3 w-32" />
                      </div>
                    </div>
                  </TableCell>
                  <TableCell><Skeleton className="h-4 w-32" /></TableCell>
                  <TableCell><Skeleton className="h-4 w-16" /></TableCell>
                  <TableCell><Skeleton className="h-5 w-14" /></TableCell>
                  <TableCell><Skeleton className="h-5 w-10" /></TableCell>
                </TableRow>
              ))
            ) : agents?.length === 0 ? (
              <TableRow>
                <TableCell colSpan={5} className="h-24 text-center text-muted-foreground">
                  No agents configured
                </TableCell>
              </TableRow>
            ) : (
              agents?.map((agent) => (
                <TableRow
                  key={agent.agent_id}
                  className="cursor-pointer"
                  onClick={() => setSelectedAgentId(agent.agent_id)}
                >
                  <TableCell className="font-medium">
                    <div className="flex items-center gap-2">
                      <span className="text-xl">{agent.emoji}</span>
                      <div className="flex flex-col">
                        <span>{agent.name}</span>
                        <span className="text-xs text-muted-foreground font-mono">{agent.agent_id}</span>
                      </div>
                    </div>
                  </TableCell>
                  <TableCell className="font-mono text-xs">{agent.primary_model}</TableCell>
                  <TableCell>
                    <div className="flex gap-1 flex-wrap">
                      {agent.tools.map((tool) => (
                        <Badge key={tool} variant="secondary" className="text-[10px] px-1">
                          {tool}
                        </Badge>
                      ))}
                    </div>
                  </TableCell>
                  <TableCell>
                    <Badge variant={agent.enabled ? "default" : "outline"} className={agent.enabled ? "bg-green-500 hover:bg-green-600" : ""}>
                      {agent.enabled ? "Active" : "Disabled"}
                    </Badge>
                  </TableCell>
                  <TableCell>
                    <Switch
                      checked={agent.enabled}
                      onCheckedChange={() => toggleAgent.mutate(agent.agent_id)}
                      disabled={toggleAgent.isPending}
                      onClick={(e) => e.stopPropagation()}
                    />
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </div>

      {selectedAgentId && (
        <AgentDetailDialog
          agentId={selectedAgentId}
          open={!!selectedAgentId}
          onOpenChange={(open) => { if (!open) setSelectedAgentId(null); }}
        />
      )}
    </div>
  );
}
