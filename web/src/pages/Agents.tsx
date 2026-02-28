import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Plus, Loader2 } from "lucide-react";
import { useAgents, useToggleAgent } from "@/hooks/use-api";
import { Switch } from "@/components/ui/switch";

export default function AgentsPage() {
  const { data: agents, isLoading } = useAgents();
  const toggleAgent = useToggleAgent();

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-2xl font-bold tracking-tight">Agents</h2>
        <Button disabled>
          <Plus className="mr-2 h-4 w-4" /> New Agent
        </Button>
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
