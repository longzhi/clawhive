"use client";

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
  data?: any;
}

export function EventStream() {
  const [events, setEvents] = useState<Event[]>([]);
  const [isConnected, setIsConnected] = useState(false);
  const [isPaused, setIsPaused] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const eventSourceRef = useRef<EventSource | null>(null);

  useEffect(() => {
    const connect = () => {
      const es = new EventSource("http://localhost:3001/api/events/stream");
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
          const data = JSON.parse(msg.data);
          const newEvent: Event = {
            type: data.type || "unknown",
            trace_id: data.trace_id || "n/a",
            timestamp: new Date().toISOString(),
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
                <Badge variant="outline" className="uppercase text-[10px] h-5 px-1">
                  {event.type}
                </Badge>
                <span className="font-mono text-muted-foreground truncate" title={event.trace_id}>
                  {event.trace_id.substring(0, 8)}
                </span>
              </div>
            ))}
          </div>
        </ScrollArea>
      </CardContent>
    </Card>
  );
}
