import { useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Pause, Play } from "lucide-react";
import { cn } from "@/lib/utils";

interface Event {
  type: string;
  trace_id: string;
  timestamp: string;
  summary: string;
  data?: any;
}

function truncateText(value: string | undefined, max = 40): string {
  if (!value) return "—";
  return value.length > max ? `${value.slice(0, max)}...` : value;
}

function getEventTypeAndPayload(data: Record<string, any>) {
  const eventType = Object.keys(data)[0] || "Unknown";
  const payload = eventType ? data[eventType] : undefined;
  return { eventType, payload };
}

function extractTraceId(eventType: string, payload: any): string {
  if (eventType === "HandleIncomingMessage") {
    return payload?.inbound?.trace_id || "—";
  }
  return payload?.trace_id || "—";
}

function buildSummary(eventType: string, payload: any, traceId: string): string {
  switch (eventType) {
    case "HandleIncomingMessage":
      return `→ ${payload?.resolved_agent_id || "—"}`;
    case "ReplyReady":
      return truncateText(payload?.outbound?.text, 40);
    case "TaskFailed":
      return truncateText(payload?.error, 40);
    case "StreamDelta":
      return "streaming...";
    case "MemoryWriteRequested":
      return `write: ${payload?.speaker || "—"}`;
    case "MemoryReadRequested":
      return `search: ${truncateText(payload?.query, 40)}`;
    case "ConsolidationCompleted":
      return `concepts: +${payload?.concepts_created ?? 0} ↑${payload?.concepts_updated ?? 0}`;
    case "ToolCallStarted":
      return payload?.tool_name || "—";
    case "ToolCallCompleted":
      return `${payload?.tool_name || "—"} (${payload?.duration_ms ?? "—"}ms)`;
    case "ScheduledTaskTriggered":
      return payload?.schedule_id || "—";
    case "ScheduledTaskCompleted":
      return `${payload?.schedule_id || "—"}: ${payload?.status || "—"}`;
    default:
      return traceId !== "—" ? traceId.slice(0, 8) : "—";
  }
}

function getEventBadgeClass(eventType: string): string {
  if (eventType === "HandleIncomingMessage" || eventType === "MessageAccepted") {
    return "border-blue-300 bg-blue-50 text-blue-700";
  }
  if (eventType === "ReplyReady") {
    return "border-green-300 bg-green-50 text-green-700";
  }
  if (eventType === "TaskFailed") {
    return "border-red-300 bg-red-50 text-red-700";
  }
  if (eventType === "StreamDelta") {
    return "border-slate-300 bg-slate-50 text-slate-700";
  }
  if (eventType.startsWith("Memory")) {
    return "border-fuchsia-300 bg-fuchsia-50 text-fuchsia-700";
  }
  if (eventType.startsWith("Tool")) {
    return "border-orange-300 bg-orange-50 text-orange-700";
  }
  if (eventType.startsWith("Scheduled")) {
    return "border-amber-300 bg-amber-50 text-amber-700";
  }
  return "";
}

export function EventStream() {
  const [events, setEvents] = useState<Event[]>([]);
  const [isConnected, setIsConnected] = useState(false);
  const [isPaused, setIsPaused] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const eventSourceRef = useRef<EventSource | null>(null);

  useEffect(() => {
    const connect = () => {
      const es = new EventSource("/api/events/stream");
      eventSourceRef.current = es;

      es.onopen = () => setIsConnected(true);
      es.onerror = () => {
        setIsConnected(false);
        es.close();
        setTimeout(connect, 3000);
      };

      es.onmessage = (msg) => {
        if (isPaused) return;
        try {
          const data = JSON.parse(msg.data) as Record<string, any>;
          const { eventType, payload } = getEventTypeAndPayload(data);
          const traceId = extractTraceId(eventType, payload);
          const newEvent: Event = {
            type: eventType,
            trace_id: traceId,
            timestamp: new Date().toISOString(),
            summary: buildSummary(eventType, payload, traceId),
            data: data
          };
          
          setEvents((prev) => {
            const next = [...prev, newEvent];
            if (next.length > 100) return next.slice(next.length - 100);
            return next;
          });
        } catch (e) {
          console.error("Failed to parse event", e);
        }
      };
    };

    connect();

    return () => {
      eventSourceRef.current?.close();
    };
  }, [isPaused]);

  useEffect(() => {
    if (!isPaused && scrollRef.current) {
      const scrollContainer = scrollRef.current.querySelector('[data-radix-scroll-area-viewport]');
      if (scrollContainer) {
        scrollContainer.scrollTop = scrollContainer.scrollHeight;
      }
    }
  }, [events, isPaused]);

  return (
    <Card className="h-[400px] flex flex-col">
      <CardHeader className="flex flex-row items-center justify-between py-3">
        <div className="flex items-center gap-2">
          <CardTitle className="text-sm font-medium">Live Events</CardTitle>
          <div className={cn("h-2 w-2 rounded-full", isConnected ? "bg-green-500" : "bg-red-500")} />
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="h-8 w-8"
          onClick={() => setIsPaused(!isPaused)}
        >
          {isPaused ? <Play className="h-4 w-4" /> : <Pause className="h-4 w-4" />}
        </Button>
      </CardHeader>
      <CardContent className="flex-1 p-0 overflow-hidden">
        <ScrollArea className="h-full" ref={scrollRef}>
          <div className="flex flex-col gap-1 p-4">
            {events.length === 0 && (
              <div className="text-center text-muted-foreground text-sm py-8">
                Waiting for events...
              </div>
            )}
            {events.map((event, i) => (
              <div key={i} className="flex items-center gap-2 text-xs border-b border-border/50 pb-1 last:border-0">
                <span className="text-muted-foreground w-16 shrink-0">
                  {new Date(event.timestamp).toLocaleTimeString()}
                </span>
                <Badge
                  variant="outline"
                  className={cn("uppercase text-[10px] h-5 px-1", getEventBadgeClass(event.type))}
                >
                  {event.type}
                </Badge>
                <span className="font-mono text-muted-foreground truncate" title={event.trace_id}>
                  {event.trace_id.substring(0, 8)}
                </span>
                <span className="text-muted-foreground truncate" title={event.summary}>
                  {event.summary}
                </span>
              </div>
            ))}
          </div>
        </ScrollArea>
      </CardContent>
    </Card>
  );
}
