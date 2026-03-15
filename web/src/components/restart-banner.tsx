import { RefreshCw, AlertTriangle, Check } from "lucide-react";
import { useConfigStatus, useReloadConfig } from "@/hooks/use-api";
import { toast } from "sonner";

export function ReloadBanner() {
  const { data: status } = useConfigStatus();
  const reload = useReloadConfig();

  if (!status?.has_pending_changes) return null;

  const handleApply = async () => {
    try {
      const result = await reload.mutateAsync();
      if (result.config_view_applied) {
        toast.success(`Config reloaded (generation ${result.generation})`);
        if (result.warnings.length > 0) {
          result.warnings.forEach((w) => toast.warning(w));
        }
      } else {
        toast.info("No changes to apply");
      }
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : "Unknown error";
      toast.error(`Reload failed: ${message}`);
    }
  };

  return (
    <div className="mx-4 sm:mx-6 mt-4 flex items-center justify-between gap-3 rounded-lg border border-amber-300 bg-amber-50 px-4 py-3 text-amber-900 dark:border-amber-700 dark:bg-amber-950/50 dark:text-amber-200">
      <div className="flex items-center gap-2">
        <AlertTriangle className="h-4 w-4 shrink-0" />
        <p className="text-sm font-medium">
          {status.changed_files.length} config change
          {status.changed_files.length !== 1 ? "s" : ""} pending
        </p>
      </div>
      <button
        onClick={handleApply}
        disabled={reload.isPending}
        className="inline-flex items-center gap-1.5 rounded-md bg-amber-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-amber-700 disabled:opacity-50 transition-colors"
      >
        {reload.isPending ? (
          <RefreshCw className="h-3 w-3 animate-spin" />
        ) : (
          <Check className="h-3 w-3" />
        )}
        Apply Changes
      </button>
    </div>
  );
}
