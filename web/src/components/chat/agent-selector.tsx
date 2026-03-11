import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { useChatAgents } from "@/hooks/use-api";
import { useChatStore } from "@/stores/chat";

export function AgentSelector() {
  const { data: agents, isLoading } = useChatAgents();
  const { selectedAgentId, setSelectedAgent } = useChatStore();

  if (isLoading) {
    return (
      <Select disabled>
        <SelectTrigger className="w-full">
          <SelectValue placeholder="Loading agents..." />
        </SelectTrigger>
      </Select>
    );
  }

  if (!agents || agents.length === 0) {
    return (
      <Select disabled>
        <SelectTrigger className="w-full">
          <SelectValue placeholder="No agents available" />
        </SelectTrigger>
      </Select>
    );
  }

  return (
    <Select value={selectedAgentId ?? undefined} onValueChange={setSelectedAgent}>
      <SelectTrigger className="w-full">
        <SelectValue placeholder="Select an agent" />
      </SelectTrigger>
      <SelectContent>
        {agents.map((agent) => (
          <SelectItem key={agent.agent_id} value={agent.agent_id}>
            {agent.name || agent.agent_id}
            {agent.model && (
              <span className="text-xs text-muted-foreground ml-2">({agent.model})</span>
            )}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
