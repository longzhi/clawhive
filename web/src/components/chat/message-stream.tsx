import { useEffect, useRef } from "react";
import Markdown from "react-markdown";

import { useChatStore, type ChatMessageItem } from "@/stores/chat";
import { cn } from "@/lib/utils";
import { Bot, User, Loader2, FileText, File as FileIcon } from "lucide-react";
import { ToolCallPanel } from "./tool-call-panel";

export function MessageStream() {
  const { conversations, activeConversationId, isProcessing } = useChatStore();
  const scrollRef = useRef<HTMLDivElement>(null);
  const isAutoScrollRef = useRef(true);

  const activeConversation = conversations.find(c => c.id === activeConversationId);
  const messages = activeConversation?.messages ?? [];

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    if (isAutoScrollRef.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [messages, isProcessing]);

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
    <div
      ref={scrollRef}
      onScroll={handleScroll}
      className="flex-1 min-h-0 overflow-y-auto"
      data-testid="chat-messages"
    >
      <div className="flex flex-col gap-4 p-4">
        {messages.map((msg, idx) => (
          <MessageBubble key={msg.id || idx} message={msg} />
        ))}
        {isProcessing && activeConversationId && <TypingIndicator />}
      </div>
    </div>
  );
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function AttachmentDisplay({ att, i, isUser }: { att: NonNullable<ChatMessageItem["attachments"]>[number]; i: number; isUser: boolean }) {
  if (att.kind === "image") {
    return (
      <a
        key={`${att.file_name}-${i}`}
        href={`/api/chat/attachments/${att.id}`}
        target="_blank"
        rel="noopener noreferrer"
        className="block"
      >
        <img
          src={`/api/chat/attachments/${att.id}`}
          alt={att.file_name}
          className={cn(
            "max-h-48 max-w-[240px] rounded border object-cover transition-opacity hover:opacity-80",
            isUser ? "border-primary-foreground/20" : "border-border"
          )}
        />
      </a>
    );
  }

  const IconComponent = att.mime_type.includes("pdf") || att.mime_type.includes("text") || att.mime_type.includes("document")
    ? FileText
    : FileIcon;

  return (
    <a
      key={`${att.file_name}-${i}`}
      href={`/api/chat/attachments/${att.id}`}
      target="_blank"
      rel="noopener noreferrer"
      className={cn(
        "flex items-center gap-2 rounded-md border px-3 py-2 text-xs transition-opacity hover:opacity-80",
        isUser ? "border-primary-foreground/20 text-primary-foreground" : "border-border"
      )}
    >
      <IconComponent className="h-4 w-4 shrink-0" />
      <span className="truncate max-w-[160px]">{att.file_name}</span>
      <span className="shrink-0 text-[10px] opacity-70">{formatFileSize(att.size)}</span>
    </a>
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
                  <AttachmentDisplay key={`${att.file_name}-${i}`} att={att} i={i} isUser />
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
                  <AttachmentDisplay key={`${att.file_name}-${i}`} att={att} i={i} isUser={false} />
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

function TypingIndicator() {
  return (
    <div className="flex gap-3 flex-row">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted">
        <Bot className="h-4 w-4" />
      </div>
      <div className="rounded-lg px-4 py-3 bg-muted">
        <div className="flex items-center gap-1">
          {[0, 1, 2].map((i) => (
            <span
              key={i}
              className="block h-2 w-2 rounded-full bg-muted-foreground/60"
              style={{
                animation: "typing-bounce 1.4s ease-in-out infinite",
                animationDelay: `${i * 0.16}s`,
              }}
            />
          ))}
        </div>
        <style>{`
          @keyframes typing-bounce {
            0%, 60%, 100% { transform: translateY(0); opacity: 0.4; }
            30% { transform: translateY(-4px); opacity: 1; }
          }
        `}</style>
      </div>
    </div>
  );
}
