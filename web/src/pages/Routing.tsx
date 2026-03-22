import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useRouting, useAgents, useUpdateRouting } from "@/hooks/use-api";
import { Loader2, Plus, Pencil, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { Skeleton } from "@/components/ui/skeleton";
import { ErrorState } from "@/components/ui/error-state";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";

interface RoutingBinding {
  channel_type: string;
  connector_id: string;
  match: { kind: string; pattern?: string };
  agent_id: string;
}

const CHANNEL_OPTIONS = ["telegram", "discord", "slack", "whatsapp", "imessage", "feishu", "dingtalk", "wecom", "weixin", "webhook"];
const MATCH_KIND_OPTIONS = ["dm", "group", "all"];

function emptyBinding(): RoutingBinding {
  return { channel_type: "telegram", connector_id: "", match: { kind: "dm" }, agent_id: "" };
}

// ---------------------------------------------------------------------------
// Routing Skeleton
// ---------------------------------------------------------------------------
function RoutingSkeleton() {
  return (
    <div className="flex flex-col gap-6">
      <Card>
        <CardHeader>
          <Skeleton className="h-5 w-36" />
          <Skeleton className="h-4 w-56 mt-1" />
        </CardHeader>
        <CardContent>
          <div className="flex items-center gap-4">
            <Skeleton className="h-4 w-24" />
            <Skeleton className="h-9 w-48" />
          </div>
        </CardContent>
      </Card>
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <Skeleton className="h-5 w-32" />
            <Skeleton className="h-4 w-56 mt-1" />
          </div>
          <Skeleton className="h-9 w-24" />
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-12 w-full" />
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export default function RoutingPage() {
  const { data: routing, isLoading: isLoadingRouting, isError: isErrorRouting, error: errorRouting, refetch: refetchRouting } = useRouting();
  const { data: agents } = useAgents();
  const updateRouting = useUpdateRouting();

  const [dialogOpen, setDialogOpen] = useState(false);
  const [editIndex, setEditIndex] = useState<number | null>(null);
  const [form, setForm] = useState<RoutingBinding>(emptyBinding());
  const [deleteIndex, setDeleteIndex] = useState<number | null>(null);

  const bindings = (routing?.bindings as RoutingBinding[] | undefined) ?? [];

  const handleDefaultAgentChange = (value: string) => {
    if (!routing) return;
    updateRouting.mutate({ ...routing, default_agent_id: value }, {
      onSuccess: () => toast.success("Default agent updated"),
      onError: () => toast.error("Failed to update default agent"),
    });
  };

  const openAdd = () => {
    setEditIndex(null);
    setForm(emptyBinding());
    setDialogOpen(true);
  };

  const openEdit = (index: number) => {
    const b = bindings[index];
    setEditIndex(index);
    setForm({ ...b, match: { ...b.match } });
    setDialogOpen(true);
  };

  const saveRule = () => {
    if (!routing) return;
    const newBindings = [...bindings];
    if (editIndex !== null) {
      newBindings[editIndex] = form;
    } else {
      newBindings.push(form);
    }
    updateRouting.mutate({ ...routing, bindings: newBindings }, {
      onSuccess: () => {
        toast.success(editIndex !== null ? "Rule updated" : "Rule added");
        setDialogOpen(false);
      },
      onError: () => toast.error("Failed to save rule"),
    });
  };

  const confirmDelete = () => {
    if (deleteIndex === null || !routing) return;
    const newBindings = bindings.filter((_, i) => i !== deleteIndex);
    updateRouting.mutate({ ...routing, bindings: newBindings }, {
      onSuccess: () => {
        toast.success("Rule deleted");
        setDeleteIndex(null);
      },
      onError: () => toast.error("Failed to delete rule"),
    });
  };

  if (isLoadingRouting) return <RoutingSkeleton />;
  if (isErrorRouting) return <ErrorState message={errorRouting?.message} onRetry={refetchRouting} />;

  return (
    <div className="flex flex-col gap-6">
      {/* Default Routing */}
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

      {/* Routing Rules */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle>Routing Rules</CardTitle>
            <CardDescription>Route messages based on patterns and sources</CardDescription>
          </div>
          <Button size="sm" onClick={openAdd}>
            <Plus className="h-4 w-4 mr-1" />
            Add Rule
          </Button>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Channel</TableHead>
                <TableHead>Connector</TableHead>
                <TableHead>Match Criteria</TableHead>
                <TableHead>Target Agent</TableHead>
                <TableHead className="w-20">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {bindings.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={5} className="text-center text-muted-foreground">
                    No routing rules configured
                  </TableCell>
                </TableRow>
              ) : (
                bindings.map((binding, i) => (
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
                    <TableCell>
                      <div className="flex gap-1">
                        <Button variant="ghost" size="icon" className="h-8 w-8" onClick={() => openEdit(i)}>
                          <Pencil className="h-3.5 w-3.5" />
                        </Button>
                        <Button variant="ghost" size="icon" className="h-8 w-8 text-destructive hover:text-destructive" onClick={() => setDeleteIndex(i)}>
                          <Trash2 className="h-3.5 w-3.5" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Add/Edit Dialog */}
      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{editIndex !== null ? "Edit Rule" : "Add Rule"}</DialogTitle>
            <DialogDescription>
              {editIndex !== null ? "Modify the routing rule" : "Create a new routing rule"}
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-4 py-2">
            <div className="grid gap-2">
              <Label>Channel Type</Label>
              <Select value={form.channel_type} onValueChange={(v) => setForm({ ...form, channel_type: v })}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {CHANNEL_OPTIONS.map((ch) => (
                    <SelectItem key={ch} value={ch} className="capitalize">{ch}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            <div className="grid gap-2">
              <Label>Connector ID</Label>
              <Input
                placeholder="e.g. my_telegram_bot"
                value={form.connector_id}
                onChange={(e) => setForm({ ...form, connector_id: e.target.value })}
              />
            </div>

            <div className="grid grid-cols-2 gap-4">
              <div className="grid gap-2">
                <Label>Match Kind</Label>
                <Select value={form.match.kind} onValueChange={(v) => setForm({ ...form, match: { ...form.match, kind: v } })}>
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {MATCH_KIND_OPTIONS.map((k) => (
                      <SelectItem key={k} value={k}>{k}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-2">
                <Label>Pattern (optional)</Label>
                <Input
                  placeholder="regex pattern"
                  value={form.match.pattern ?? ""}
                  onChange={(e) => setForm({ ...form, match: { ...form.match, pattern: e.target.value || undefined } })}
                />
              </div>
            </div>

            <div className="grid gap-2">
              <Label>Target Agent</Label>
              <Select value={form.agent_id} onValueChange={(v) => setForm({ ...form, agent_id: v })}>
                <SelectTrigger>
                  <SelectValue placeholder="Select agent" />
                </SelectTrigger>
                <SelectContent>
                  {agents?.map((agent) => (
                    <SelectItem key={agent.agent_id} value={agent.agent_id}>
                      {agent.name} ({agent.agent_id})
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDialogOpen(false)}>Cancel</Button>
            <Button
              onClick={saveRule}
              disabled={updateRouting.isPending || !form.connector_id || !form.agent_id}
            >
              {updateRouting.isPending && <Loader2 className="h-4 w-4 mr-2 animate-spin" />}
              {editIndex !== null ? "Update" : "Add"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Delete Confirmation Dialog */}
      <ConfirmDialog
        open={deleteIndex !== null}
        onOpenChange={(open) => { if (!open) setDeleteIndex(null); }}
        title="Delete Rule"
        description={
          deleteIndex !== null && bindings[deleteIndex]
            ? `Delete routing rule "${bindings[deleteIndex].channel_type} / ${bindings[deleteIndex].connector_id} → ${bindings[deleteIndex].agent_id}"? This action cannot be undone.`
            : "Are you sure you want to delete this routing rule? This action cannot be undone."
        }
        confirmLabel="Delete"
        variant="destructive"
        loading={updateRouting.isPending}
        onConfirm={confirmDelete}
      />
    </div>
  );
}
