import { useState, useRef, useCallback } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Send, Square, Loader2 } from "lucide-react";
import { useChatStore } from "@/stores/chat";

interface MessageInputProps {
  onSend: (text: string) => void;
  onCancel: () => void;
}

export function MessageInput({ onSend, onCancel }: MessageInputProps) {
  const [text, setText] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const { isProcessing, isConnected } = useChatStore();

  const MAX_LENGTH = 10000;

  const handleSend = useCallback(() => {
    const trimmed = text.trim();
    if (!trimmed || isProcessing || !isConnected) return;
    onSend(trimmed);
    setText("");
    textareaRef.current?.focus();
  }, [text, isProcessing, isConnected, onSend]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  return (
    <div className="border-t p-4" data-testid="chat-input">
      <div className="flex gap-2 items-end">
        <div className="flex-1 relative">
          <Textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value.slice(0, MAX_LENGTH))}
            onKeyDown={handleKeyDown}
            placeholder={isConnected ? "Type a message... (Shift+Enter for new line)" : "Disconnected..."}
            disabled={!isConnected}
            className="min-h-[44px] max-h-[200px] resize-none pr-16"
            rows={1}
          />
          {text.length > MAX_LENGTH * 0.9 && (
            <span className="absolute bottom-2 right-2 text-xs text-muted-foreground">
              {text.length}/{MAX_LENGTH}
            </span>
          )}
        </div>
        {isProcessing ? (
          <Button variant="destructive" size="icon" onClick={onCancel} title="Cancel">
            <Square className="h-4 w-4" />
          </Button>
        ) : (
          <Button
            size="icon"
            onClick={handleSend}
            disabled={!text.trim() || !isConnected}
            title="Send"
          >
            {!isConnected ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <Send className="h-4 w-4" />
            )}
          </Button>
        )}
      </div>
    </div>
  );
}
