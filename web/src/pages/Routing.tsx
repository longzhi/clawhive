import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { useRouting, useAgents, useUpdateRouting } from "@/hooks/use-api";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

export default function RoutingPage() {
  const { data: routing, isLoading: isLoadingRouting } = useRouting();
  const { data: agents } = useAgents();
  const updateRouting = useUpdateRouting();

  const handleDefaultAgentChange = (value: string) => {
    if (!routing) return;
    updateRouting.mutate({ ...routing, default_agent_id: value }, {
      onSuccess: () => toast.success("Default agent updated"),
      onError: () => toast.error("Failed to update default agent")
    });
  };

  if (isLoadingRouting) {
    return (
      <div className="flex justify-center p-8">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <Card>
        <CardHeader>
          <CardTitle>Default Routing</CardTitle>
          <CardDescription>Fallback agent when no rules match</CardDescription>
        </CardHeader>
        <CardContent>
          <div className="flex items-center gap-4">
            <span className="text-sm font-medium whitespace-nowrap">Default Agent:</span>
            <Select
              value={routing?.default_agent_id as string | undefined}
              onValueChange={handleDefaultAgentChange}
              disabled={updateRouting.isPending}
            >
              <SelectTrigger className="w-[200px]">
                <SelectValue placeholder="Select agent" />
              </SelectTrigger>
              <SelectContent>
                {agents?.map((agent) => (
                  <SelectItem key={agent.agent_id} value={agent.agent_id}>
                    {agent.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Routing Rules</CardTitle>
          <CardDescription>Route messages based on patterns and sources</CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Channel</TableHead>
                <TableHead>Connector</TableHead>
                <TableHead>Match Criteria</TableHead>
                <TableHead>Target Agent</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(routing?.bindings as any[] | undefined)?.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={4} className="text-center text-muted-foreground">
                    No routing rules configured
                  </TableCell>
                </TableRow>
              ) : (
                (routing?.bindings as any[] | undefined)?.map((binding: any, i: number) => (
                  <TableRow key={i}>
                    <TableCell className="capitalize">{binding.channel_type}</TableCell>
                    <TableCell className="font-mono text-xs">{binding.connector_id}</TableCell>
                    <TableCell>
                      <div className="flex flex-col gap-1">
                        <Badge variant="outline" className="w-fit">
                          kind: {binding.match.kind}
                        </Badge>
                        {binding.match.pattern && (
                          <span className="font-mono text-xs text-muted-foreground">
                            pattern: {binding.match.pattern}
                          </span>
                        )}
                      </div>
                    </TableCell>
                    <TableCell className="font-medium">{binding.agent_id}</TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </div>
  );
}
