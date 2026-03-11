import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useChatAgents } from "@/hooks/use-api";
import { useChatWebSocket } from "@/hooks/use-chat-ws";
import { useChatStore } from "@/stores/chat";
import { cn } from "@/lib/utils";
import { PanelLeftClose, PanelLeftOpen, Bot, WifiOff } from "lucide-react";
import { ConversationSidebar } from "@/components/chat/conversation-sidebar";
import { MessageStream } from "@/components/chat/message-stream";
import { MessageInput } from "@/components/chat/message-input";

export default function Chat() {
  const { data: agents } = useChatAgents();
  const {
    activeConversationId,
    selectedAgentId,
    setSelectedAgent,
    isConnected,
    addMessage,
  } = useChatStore();

  const { sendMessage, cancelRequest } = useChatWebSocket();
  const [sidebarOpen, setSidebarOpen] = useState(true);

  // Auto-select first agent on load
  useEffect(() => {
    if (!selectedAgentId && agents && agents.length > 0) {
      setSelectedAgent(agents[0].agent_id);
    }
  }, [agents, selectedAgentId, setSelectedAgent]);

  const activeAgent = agents?.find((a) => a.agent_id === selectedAgentId);

  const handleSend = (text: string) => {
    if (!activeConversationId || !selectedAgentId) return;
    const { pendingAttachments, clearPendingAttachments } = useChatStore.getState();

    addMessage(activeConversationId, {
      id: `user-${Date.now()}`,
      role: "user",
      text,
      timestamp: new Date().toISOString(),
      tool_calls: [],
      is_streaming: false,
      attachments: pendingAttachments.length > 0 ? [...pendingAttachments] : undefined,
    });

    sendMessage(
      text,
      selectedAgentId,
      activeConversationId,
      pendingAttachments.length > 0 ? pendingAttachments : undefined,
    );
    clearPendingAttachments();
  };

  return (
    <div className="flex h-[calc(100vh-8rem)] gap-0 overflow-hidden rounded-xl border bg-background shadow-sm">
      {/* Left sidebar — conversation list */}
      <div
        className={cn(
          "flex flex-col border-r bg-muted/30 transition-all duration-200",
          sidebarOpen ? "w-[280px] min-w-[280px]" : "w-0 min-w-0 overflow-hidden border-r-0"
        )}
      >
        <ConversationSidebar />
      </div>

      {/* Center — messages + input */}
      <div className="flex flex-1 flex-col min-w-0">
        {/* Header */}
        <div className="flex items-center gap-2 border-b px-4 py-2.5">
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7 shrink-0"
            onClick={() => setSidebarOpen(!sidebarOpen)}
            title={sidebarOpen ? "Collapse sidebar" : "Expand sidebar"}
          >
            {sidebarOpen ? (
              <PanelLeftClose className="h-4 w-4" />
            ) : (
              <PanelLeftOpen className="h-4 w-4" />
            )}
          </Button>
          {activeConversationId ? (
            <div className="flex items-center gap-2 min-w-0">
              <Bot className="h-4 w-4 text-muted-foreground shrink-0" />
              <span className="text-sm font-medium truncate">
                {activeAgent?.name ?? selectedAgentId ?? "Agent"}
              </span>
              {activeAgent?.model && (
                <Badge variant="secondary" className="text-[10px] h-5 px-1.5 shrink-0">
                  {activeAgent.model}
                </Badge>
              )}
            </div>
          ) : (
            <span className="text-sm text-muted-foreground">Select or start a conversation</span>
          )}
          <div className="ml-auto">
            {!isConnected && (
              <div className="flex items-center gap-1.5 text-destructive">
                <WifiOff className="h-4 w-4" />
                <span className="text-xs">Disconnected</span>
              </div>
            )}
          </div>
        </div>

        {/* Message stream area */}
        <MessageStream />

        {/* Input area */}
        <MessageInput onSend={handleSend} onCancel={cancelRequest} />
      </div>
    </div>
  );
}
