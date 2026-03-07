import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Skeleton } from "@/components/ui/skeleton";
import { EmptyState } from "@/components/ui/empty-state";
import { ErrorState } from "@/components/ui/error-state";
import { Package, ShieldCheck, Plus, Loader2, AlertTriangle } from "lucide-react";
import {
  useSkills,
  useAnalyzeSkill,
  useInstallSkill,
  type AnalyzeSkillResponse,
} from "@/hooks/use-api";
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
// Severity badge helper
// ---------------------------------------------------------------------------
function SeverityBadge({ severity }: { severity: string }) {
  const lower = severity.toLowerCase();
  if (lower === "high") return <Badge className="bg-red-500 hover:bg-red-600 text-white text-[10px] px-1">{severity}</Badge>;
  if (lower === "medium") return <Badge className="bg-amber-500 hover:bg-amber-600 text-white text-[10px] px-1">{severity}</Badge>;
  return <Badge className="bg-blue-500 hover:bg-blue-600 text-white text-[10px] px-1">{severity}</Badge>;
}

// ---------------------------------------------------------------------------
// Install Skill Dialog
// ---------------------------------------------------------------------------
function InstallSkillDialog({ onInstalled }: { onInstalled: () => void }) {
  const [open, setOpen] = useState(false);
  const [source, setSource] = useState("");
  const [allowHighRisk, setAllowHighRisk] = useState(false);
  const [report, setReport] = useState<AnalyzeSkillResponse | null>(null);

  const analyze = useAnalyzeSkill();
  const install = useInstallSkill();

  const reset = () => {
    setSource("");
    setAllowHighRisk(false);
    setReport(null);
    analyze.reset();
    install.reset();
  };

  const handleAnalyze = async () => {
    if (!source.trim()) return;
    setReport(null);
    try {
      const result = await analyze.mutateAsync(source.trim());
      setReport(result);
    } catch {
      toast.error("Analysis failed");
    }
  };

  const handleInstall = async () => {
    if (!source.trim()) return;
    try {
      const result = await install.mutateAsync({ source: source.trim(), allowHighRisk });
      toast.success(`Skill "${result.skill_name}" installed`);
      onInstalled();
      reset();
      setOpen(false);
    } catch {
      toast.error("Installation failed");
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); if (!v) reset(); }}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="mr-2 h-4 w-4" /> Install Skill
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>Install Skill</DialogTitle>
          <DialogDescription>Enter a local path or URL to a skill to analyze and install it.</DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          {/* Source input */}
          <div>
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Skill Source
            </label>
            <div className="mt-1 flex gap-2">
              <Input
                placeholder="/path/to/skill or https://..."
                value={source}
                onChange={(e) => setSource(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleAnalyze()}
              />
              <Button
                variant="outline"
                onClick={handleAnalyze}
                disabled={!source.trim() || analyze.isPending}
              >
                {analyze.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Analyze"}
              </Button>
            </div>
          </div>

          {/* Analysis report */}
          {report && (
            <div className="space-y-3 rounded-md border bg-muted/30 p-4">
              <div>
                <p className="font-semibold">{report.skill_name}</p>
                <p className="text-sm text-muted-foreground">{report.description}</p>
              </div>

              {/* Findings */}
              {report.findings.length > 0 && (
                <div className="space-y-1.5">
                  <p className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
                    Findings ({report.findings.length})
                  </p>
                  <div className="space-y-1">
                    {report.findings.map((f, i) => (
                      <div
                        key={i}
                        className="flex items-start gap-2 rounded-sm border bg-background px-3 py-2 text-sm"
                      >
                        <SeverityBadge severity={f.severity} />
                        <div className="min-w-0">
                          <span className="font-mono text-xs text-muted-foreground">
                            {f.file}:{f.line}
                          </span>
                          <span className="mx-1 text-muted-foreground">—</span>
                          <span className="font-mono text-xs">{f.pattern}</span>
                          <p className="text-xs text-muted-foreground">{f.reason}</p>
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Rendered report */}
              <div>
                <p className="text-xs font-medium text-muted-foreground uppercase tracking-wide mb-1">
                  Report
                </p>
                <pre className="max-h-48 overflow-auto rounded-sm border bg-background p-3 text-xs font-mono whitespace-pre-wrap">
                  {report.rendered_report}
                </pre>
              </div>

              {/* High risk warning */}
              {report.has_high_risk && (
                <div className="flex items-start gap-3 rounded-md border border-amber-500/40 bg-amber-500/10 px-4 py-3">
                  <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
                  <div className="flex-1">
                    <p className="text-sm font-medium text-amber-700 dark:text-amber-400">
                      High-risk findings detected
                    </p>
                    <p className="text-xs text-amber-600 dark:text-amber-500 mt-0.5">
                      This skill contains potentially dangerous patterns. Only install if you trust the source.
                    </p>
                  </div>
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-muted-foreground">Allow</span>
                    <Switch
                      checked={allowHighRisk}
                      onCheckedChange={setAllowHighRisk}
                    />
                  </div>
                </div>
              )}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button
            onClick={handleInstall}
            disabled={
              !source.trim() ||
              !report ||
              install.isPending ||
              (report.has_high_risk && !allowHighRisk)
            }
          >
            {install.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Install"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Skeleton rows for loading state
// ---------------------------------------------------------------------------
function SkillTableSkeleton() {
  return (
    <>
      {Array.from({ length: 4 }).map((_, i) => (
        <TableRow key={i}>
          <TableCell>
            <Skeleton className="h-4 w-32" />
          </TableCell>
          <TableCell>
            <Skeleton className="h-4 w-56" />
          </TableCell>
          <TableCell>
            <Skeleton className="h-5 w-16 rounded-full" />
          </TableCell>
        </TableRow>
      ))}
    </>
  );
}

// ---------------------------------------------------------------------------
// Main Page
// ---------------------------------------------------------------------------
export default function SkillsPage() {
  const { data: skills, isLoading, isError, error, refetch } = useSkills();

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-2xl font-bold tracking-tight">Skills</h2>
        <InstallSkillDialog onInstalled={() => refetch()} />
      </div>

      {isError ? (
        <ErrorState
          title="Failed to load skills"
          message={(error as Error)?.message}
          onRetry={() => refetch()}
        />
      ) : (
        <div className="rounded-md border bg-card">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Description</TableHead>
                <TableHead>Permissions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {isLoading ? (
                <SkillTableSkeleton />
              ) : skills?.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={3} className="p-0">
                    <EmptyState
                      icon={<Package className="h-10 w-10" />}
                      title="No skills installed"
                      description="Skills extend your agents with extra tools and capabilities."
                      action={{
                        label: "Install Skill",
                        onClick: () => {
                          // Trigger the dialog — handled via button in header
                          document
                            .querySelector<HTMLButtonElement>('[data-install-trigger]')
                            ?.click();
                        },
                      }}
                    />
                  </TableCell>
                </TableRow>
              ) : (
                skills?.map((skill) => (
                  <TableRow key={skill.path}>
                    <TableCell className="font-medium font-mono text-sm">
                      {skill.name}
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {skill.description || <span className="italic">No description</span>}
                    </TableCell>
                    <TableCell>
                      {skill.has_permissions ? (
                        <Badge
                          variant="outline"
                          className="gap-1 border-amber-500/40 text-amber-600 dark:text-amber-400"
                        >
                          <ShieldCheck className="h-3 w-3" />
                          Permissions
                        </Badge>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}
