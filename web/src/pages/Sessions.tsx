import { useState } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { useSessions, useSessionMessages } from "@/hooks/use-api";
import { cn } from "@/lib/utils";
import { Loader2, ArrowLeft } from "lucide-react";
import { Button } from "@/components/ui/button";

export default function SessionsPage() {
  const { data: sessions, isLoading: isLoadingSessions } = useSessions();
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const { data: messages, isLoading: isLoadingMessages } = useSessionMessages(selectedKey || "");

  const selectedSession = sessions?.find(s => s.session_key === selectedKey);

  return (
    <div className="flex flex-col md:flex-row h-[calc(100vh-8rem)] gap-4">
      <Card className={cn("w-full md:w-1/3 flex flex-col h-full", selectedKey ? "hidden md:flex" : "flex")}>
        <CardHeader className="pb-3">
          <CardTitle>Sessions</CardTitle>
          <CardDescription>Recent conversations</CardDescription>
        </CardHeader>
        <Separator />
        <ScrollArea className="flex-1">
          <div className="flex flex-col gap-2 p-4">
            {isLoadingSessions ? (
              <div className="flex justify-center p-4">
                <Loader2 className="h-6 w-6 animate-spin" />
              </div>
            ) : sessions?.length === 0 ? (
              <div className="text-center text-muted-foreground p-4">No sessions found</div>
            ) : (
              sessions?.map((session) => (
                <div
                  key={session.session_key}
                  onClick={() => setSelectedKey(session.session_key)}
                  className={cn(
                    "flex flex-col items-start gap-2 rounded-lg border p-3 text-left text-sm transition-all hover:bg-accent cursor-pointer",
                    selectedKey === session.session_key ? "bg-accent" : ""
                  )}
                >
                  <div className="flex w-full flex-col gap-1">
                    <div className="flex items-center justify-between w-full">
                      <div className="font-semibold truncate max-w-[150px]">{session.session_key}</div>
                      <div className="text-xs text-muted-foreground">
                        {new Date(session.last_modified).toLocaleDateString()}
                      </div>
                    </div>
                    <div className="flex items-center gap-2 text-xs text-muted-foreground">
                      <Badge variant="secondary" className="text-[10px] h-5 px-1">
                        {session.message_count} msgs
                      </Badge>
                      <span className="truncate max-w-[150px]">{session.file_name}</span>
                    </div>
                  </div>
                </div>
              ))
            )}
          </div>
        </ScrollArea>
      </Card>

      <Card className={cn("flex flex-col flex-1 h-full", selectedKey ? "flex" : "hidden md:flex")}>
        {selectedKey ? (
          <>
            <CardHeader className="pb-3 border-b flex flex-row items-center gap-2">
              <Button variant="ghost" size="icon" className="md:hidden h-8 w-8" onClick={() => setSelectedKey(null)}>
                <ArrowLeft className="h-4 w-4" />
              </Button>
              <div className="flex flex-col">
                <CardTitle className="text-base">{selectedSession?.session_key}</CardTitle>
                <CardDescription className="text-xs truncate max-w-[200px] md:max-w-md">
                  {selectedSession?.file_name}
                </CardDescription>
              </div>
            </CardHeader>
            <ScrollArea className="flex-1 p-4">
              {isLoadingMessages ? (
                <div className="flex justify-center p-8">
                  <Loader2 className="h-8 w-8 animate-spin" />
                </div>
              ) : (
                <div className="flex flex-col gap-4">
                  {messages?.map((msg, i) => (
                    <div key={i} className={cn("flex gap-3", msg.role === "user" ? "flex-row-reverse" : "flex-row")}>
                      <div className={cn(
                        "h-8 w-8 rounded-full flex items-center justify-center text-xs font-bold shrink-0",
                        msg.role === "user" ? "bg-primary text-primary-foreground" : "bg-muted"
                      )}>
                        {msg.role === "user" ? "U" : "AI"}
                      </div>
                      <div className={cn("grid gap-1 max-w-[80%]", msg.role === "user" ? "text-right" : "text-left")}>
                        <div className="font-semibold text-xs text-muted-foreground">
                          {msg.role === "user" ? "User" : "Agent"} • {new Date(msg.timestamp).toLocaleTimeString()}
                        </div>
                        <div className={cn(
                          "text-sm p-3 rounded-md whitespace-pre-wrap",
                          msg.role === "user"
                            ? "bg-primary text-primary-foreground"
                            : "bg-muted text-foreground"
                        )}>
                          {msg.text}
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </ScrollArea>
          </>
        ) : (
          <div className="flex flex-col items-center justify-center h-full text-muted-foreground">
            <p>Select a session to view messages</p>
          </div>
        )}
      </Card>
    </div>
  );
}
