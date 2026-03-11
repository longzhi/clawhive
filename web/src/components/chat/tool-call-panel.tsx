import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ChevronDown, ChevronRight, Loader2, Wrench } from "lucide-react";
import type { ToolCallInfo } from "@/stores/chat";

interface ToolCallPanelProps {
  toolCalls: ToolCallInfo[];
}

export function ToolCallPanel({ toolCalls }: ToolCallPanelProps) {
  if (toolCalls.length === 0) return null;

  return (
    <div className="flex flex-col gap-1 my-2">
      {toolCalls.map((tc, idx) => (
        <ToolCallItem key={`${tc.tool_name}-${idx}`} toolCall={tc} />
      ))}
    </div>
  );
}

function ToolCallItem({ toolCall }: { toolCall: ToolCallInfo }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className="rounded-md border bg-background/50 text-sm">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex items-center gap-2 w-full px-3 py-2 hover:bg-muted/50 transition-colors"
      >
        {toolCall.is_running ? (
          <Loader2 className="h-3.5 w-3.5 animate-spin text-orange-500" />
        ) : expanded ? (
          <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
        )}
        <Wrench className="h-3.5 w-3.5 text-muted-foreground" />
        <Badge variant="outline" className="text-xs font-mono">
          {toolCall.tool_name}
        </Badge>
        {toolCall.is_running && (
          <span className="text-xs text-muted-foreground">executing...</span>
        )}
        {toolCall.duration_ms !== undefined && (
          <Badge variant="secondary" className="text-xs ml-auto">
            {toolCall.duration_ms < 1000
              ? `${toolCall.duration_ms}ms`
              : `${(toolCall.duration_ms / 1000).toFixed(1)}s`}
          </Badge>
        )}
      </button>
      {expanded && (
        <div className="px-3 pb-3 space-y-2">
          {toolCall.arguments && (
            <div>
              <p className="text-xs font-medium text-muted-foreground mb-1">Arguments</p>
              <pre className="text-xs bg-muted rounded p-2 overflow-x-auto max-h-[200px] overflow-y-auto">
                {formatJson(toolCall.arguments)}
              </pre>
            </div>
          )}
          {toolCall.output && (
            <div>
              <p className="text-xs font-medium text-muted-foreground mb-1">Output</p>
              <ExpandableOutput text={toolCall.output} />
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function ExpandableOutput({ text }: { text: string }) {
  const [showFull, setShowFull] = useState(false);
  const truncateAt = 500;
  const needsTruncation = text.length > truncateAt;
  const displayText = needsTruncation && !showFull ? text.slice(0, truncateAt) + "..." : text;

  return (
    <div>
      <pre className="text-xs bg-muted rounded p-2 overflow-x-auto max-h-[200px] overflow-y-auto whitespace-pre-wrap">
        {displayText}
      </pre>
      {needsTruncation && (
        <Button
          variant="ghost"
          size="sm"
          className="text-xs h-6 mt-1"
          onClick={() => setShowFull(!showFull)}
        >
          {showFull ? "Show less" : "Show more"}
        </Button>
      )}
    </div>
  );
}

function formatJson(str: string): string {
  try {
    return JSON.stringify(JSON.parse(str), null, 2);
  } catch {
    return str;
  }
}
