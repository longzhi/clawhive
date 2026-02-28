import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Plus, Loader2 } from "lucide-react";
import { useAgents, useToggleAgent, useCreateAgent, useProviders } from "@/hooks/use-api";
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

const EMOJI_OPTIONS = ["\u{1F41D}", "\u{1F916}", "\u{1F9E0}", "\u{26A1}", "\u{1F680}", "\u{1F4A1}", "\u{1F33F}", "\u{1F525}"];

// ---------------------------------------------------------------------------
// New Agent Dialog
// ---------------------------------------------------------------------------
function NewAgentDialog() {
  const [open, setOpen] = useState(false);
  const [agentId, setAgentId] = useState("");
  const [name, setName] = useState("");
  const [emoji, setEmoji] = useState("\u{1F916}");
  const [selectedModel, setSelectedModel] = useState("");
  const createAgent = useCreateAgent();
  const { data: providers } = useProviders();

  // Collect all models from configured providers
  const allModels = providers?.flatMap((p) => p.models) ?? [];

  const reset = () => {
    setAgentId("");
    setName("");
    setEmoji("\u{1F916}");
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
// Main Page
// ---------------------------------------------------------------------------
export default function AgentsPage() {
  const { data: agents, isLoading } = useAgents();
  const toggleAgent = useToggleAgent();

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
              <TableRow>
                <TableCell colSpan={5} className="h-24 text-center">
                  <div className="flex justify-center">
                    <Loader2 className="h-6 w-6 animate-spin" />
                  </div>
                </TableCell>
              </TableRow>
            ) : agents?.length === 0 ? (
              <TableRow>
                <TableCell colSpan={5} className="h-24 text-center text-muted-foreground">
                  No agents configured
                </TableCell>
              </TableRow>
            ) : (
              agents?.map((agent) => (
                <TableRow key={agent.agent_id}>
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
                    />
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
