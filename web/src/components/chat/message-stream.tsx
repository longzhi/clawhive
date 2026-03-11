import { useEffect, useRef } from "react";
import Markdown from "react-markdown";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useChatStore, type ChatMessageItem } from "@/stores/chat";
import { cn } from "@/lib/utils";
import { Bot, User, Loader2 } from "lucide-react";
import { ToolCallPanel } from "./tool-call-panel";

export function MessageStream() {
  const { conversations, activeConversationId } = useChatStore();
  const scrollRef = useRef<HTMLDivElement>(null);
  const isAutoScrollRef = useRef(true);

  const activeConversation = conversations.find(c => c.id === activeConversationId);
  const messages = activeConversation?.messages ?? [];

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    if (isAutoScrollRef.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [messages]);

  // Track if user scrolled up
  const handleScroll = () => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    isAutoScrollRef.current = scrollHeight - scrollTop - clientHeight < 100;
  };

  if (!activeConversationId) {
    return (
      <div className="flex-1 flex items-center justify-center text-muted-foreground" data-testid="chat-messages">
        <div className="text-center">
          <Bot className="h-12 w-12 mx-auto mb-4 opacity-50" />
          <p className="text-lg font-medium">Start a conversation</p>
          <p className="text-sm mt-1">Select an agent and create a new conversation to get started.</p>
        </div>
      </div>
    );
  }

  if (messages.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center text-muted-foreground" data-testid="chat-messages">
        <div className="text-center">
          <p className="text-sm">Send a message to start the conversation.</p>
        </div>
      </div>
    );
  }

  return (
    <ScrollArea className="flex-1 min-h-0" data-testid="chat-messages" ref={scrollRef} onScrollCapture={handleScroll}>
      <div className="flex flex-col gap-4 p-4">
        {messages.map((msg, idx) => (
          <MessageBubble key={msg.id || idx} message={msg} />
        ))}
      </div>
    </ScrollArea>
  );
}

function MessageBubble({ message }: { message: ChatMessageItem }) {
  const isUser = message.role === "user";

  return (
    <div className={cn("flex gap-3", isUser ? "flex-row-reverse" : "flex-row")}>
      <div className={cn(
        "flex h-8 w-8 shrink-0 items-center justify-center rounded-full",
        isUser ? "bg-primary text-primary-foreground" : "bg-muted"
      )}>
        {isUser ? <User className="h-4 w-4" /> : <Bot className="h-4 w-4" />}
      </div>
      <div className={cn(
        "max-w-[80%] rounded-lg px-4 py-2",
        isUser
          ? "bg-primary text-primary-foreground"
          : "bg-muted"
      )}>
        {isUser ? (
          <>
            {message.attachments && message.attachments.length > 0 && (
              <div className="flex flex-wrap gap-1.5 mb-1.5">
                {message.attachments.map((att, i) => (
                  <a
                    key={`${att.file_name ?? "img"}-${i}`}
                    href={`data:${att.mime_type};base64,${att.data}`}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="block"
                  >
                    <img
                      src={`data:${att.mime_type};base64,${att.data}`}
                      alt={att.file_name ?? "attachment"}
                      className="max-h-48 max-w-[240px] rounded border border-primary-foreground/20 object-cover transition-opacity hover:opacity-80"
                    />
                  </a>
                ))}
              </div>
            )}
            {message.text && <p className="text-sm whitespace-pre-wrap">{message.text}</p>}
          </>
        ) : (
          <>
            {message.attachments && message.attachments.length > 0 && (
              <div className="flex flex-wrap gap-1.5 mb-1.5">
                {message.attachments.map((att, i) => (
                  <a
                    key={`${att.file_name ?? "img"}-${i}`}
                    href={`data:${att.mime_type};base64,${att.data}`}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="block"
                  >
                    <img
                      src={`data:${att.mime_type};base64,${att.data}`}
                      alt={att.file_name ?? "attachment"}
                      className="max-h-48 max-w-[240px] rounded border border-border object-cover transition-opacity hover:opacity-80"
                    />
                  </a>
                ))}
              </div>
            )}
            {message.tool_calls.length > 0 && (
              <ToolCallPanel toolCalls={message.tool_calls} />
            )}
            <div className="prose prose-sm dark:prose-invert max-w-none">
              <Markdown>{message.text}</Markdown>
            </div>
          </>
        )}
        {message.is_streaming && (
          <div className="flex items-center gap-1 mt-1">
            <Loader2 className="h-3 w-3 animate-spin" />
            <span className="text-xs opacity-70">Streaming...</span>
          </div>
        )}
        <div className={cn("text-xs mt-1", isUser ? "text-primary-foreground/70" : "text-muted-foreground")}>
          {new Date(message.timestamp).toLocaleTimeString()}
        </div>
      </div>
    </div>
  );
}
