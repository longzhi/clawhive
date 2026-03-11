import { useState } from "react";
import { ChevronDown, ChevronRight, Brain } from "lucide-react";

interface ThinkingPanelProps {
  text: string;
  durationMs?: number;
}

export function ThinkingPanel({ text, durationMs }: ThinkingPanelProps) {
  const [expanded, setExpanded] = useState(false);

  if (!text) return null;

  const label = durationMs
    ? `Thought for ${(durationMs / 1000).toFixed(1)}s`
    : "Thinking...";

  return (
    <div className="rounded-md border bg-background/50 text-sm my-2">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex items-center gap-2 w-full px-3 py-2 hover:bg-muted/50 transition-colors"
      >
        {expanded ? (
          <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
        )}
        <Brain className="h-3.5 w-3.5 text-purple-500" />
        <span className="text-xs text-muted-foreground">{label}</span>
      </button>
      {expanded && (
        <div className="px-3 pb-3">
          <pre className="text-xs bg-muted rounded p-2 overflow-x-auto max-h-[300px] overflow-y-auto whitespace-pre-wrap">
            {text}
          </pre>
        </div>
      )}
    </div>
  );
}
