import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Plus, Trash2, MessageSquare } from "lucide-react";
import { cn } from "@/lib/utils";
import { useChatStore } from "@/stores/chat";
import { useChatConversations, useCreateChatConversation, useDeleteChatConversation } from "@/hooks/use-api";
import { AgentSelector } from "./agent-selector";
import { useEffect } from "react";

export function ConversationSidebar() {
  const { activeConversationId, setActiveConversation, selectedAgentId, conversations, setConversations } = useChatStore();
  const { data: serverConversations, isLoading } = useChatConversations();
  const createConversation = useCreateChatConversation();
  const deleteConversation = useDeleteChatConversation();

  // Sync server conversations to store
  useEffect(() => {
    if (serverConversations) {
      const { conversations: existing } = useChatStore.getState();
      const existingMap = new Map(existing.map((c) => [c.id, c]));
      setConversations(
        serverConversations.map((sc) => {
          const prev = existingMap.get(sc.conversation_id);
          return {
            id: sc.conversation_id,
            agent_id: sc.agent_id,
            title: sc.preview || `Chat with ${sc.agent_id}`,
            last_message_at: sc.last_message_at,
            messages: prev?.messages ?? [],
          };
        })
      );
    }
  }, [serverConversations, setConversations]);

  const handleCreate = async () => {
    if (!selectedAgentId) return;
    try {
      const result = await createConversation.mutateAsync({ agent_id: selectedAgentId });
      // Add to local store immediately
      const newConv = {
        id: result.conversation_id,
        agent_id: result.agent_id,
        title: `Chat with ${result.agent_id}`,
        last_message_at: null,
        messages: [],
      };
      setConversations([newConv, ...conversations]);
      setActiveConversation(result.conversation_id);
    } catch (e) {
      console.error("Failed to create conversation:", e);
    }
  };

  const handleDelete = async (e: React.MouseEvent, conversationId: string) => {
    e.stopPropagation();
    try {
      await deleteConversation.mutateAsync(conversationId);
      if (activeConversationId === conversationId) {
        setActiveConversation(null);
      }
    } catch (e) {
      console.error("Failed to delete conversation:", e);
    }
  };

  return (
    <div className="flex flex-col h-full" data-testid="chat-sidebar">
      <div className="p-3 space-y-2">
        <AgentSelector />
        <Button
          className="w-full"
          size="sm"
          onClick={handleCreate}
          disabled={!selectedAgentId || createConversation.isPending}
        >
          <Plus className="h-4 w-4 mr-2" />
          New Conversation
        </Button>
      </div>

      <Separator />

      <ScrollArea className="flex-1">
        <div className="p-2 space-y-1">
          {isLoading ? (
            <div className="p-4 text-center text-sm text-muted-foreground">
              Loading...
            </div>
          ) : conversations.length === 0 ? (
            <div className="p-4 text-center text-sm text-muted-foreground">
              No conversations yet. Create one to get started.
            </div>
          ) : (
            conversations.map((conv) => (
              <button
                key={conv.id}
                onClick={() => setActiveConversation(conv.id)}
                className={cn(
                  "flex items-start gap-2 w-full rounded-md px-3 py-2 text-left text-sm transition-colors group",
                  activeConversationId === conv.id
                    ? "bg-accent text-accent-foreground"
                    : "hover:bg-muted"
                )}
              >
                <MessageSquare className="h-4 w-4 mt-0.5 shrink-0 text-muted-foreground" />
                <div className="flex-1 min-w-0">
                  <p className="font-medium truncate">{conv.title}</p>
                  <p className="text-xs text-muted-foreground truncate">
                    {conv.agent_id}
                    {conv.last_message_at && (
                      <> · {new Date(conv.last_message_at).toLocaleDateString()}</>
                    )}
                  </p>
                </div>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6 opacity-0 group-hover:opacity-100 shrink-0"
                  onClick={(e) => handleDelete(e, conv.id)}
                  title="Delete conversation"
                >
                  <Trash2 className="h-3 w-3" />
                </Button>
              </button>
            ))
          )}
        </div>
      </ScrollArea>
    </div>
  );
}
