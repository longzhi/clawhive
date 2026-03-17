import { useState, useRef, useCallback, useMemo, useEffect, type DragEvent, type ClipboardEvent } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Send, Square, Loader2, Paperclip, X, FileText, File as FileIcon } from "lucide-react";
import { useChatStore } from "@/stores/chat";
import { uploadAttachment } from "@/lib/api";
import { cn } from "@/lib/utils";

const MAX_LENGTH = 10000;
const MAX_ATTACHMENTS = 5;
const MAX_FILE_SIZE = 20 * 1024 * 1024;

const SLASH_COMMANDS = [
  { command: "/new", args: "[model]", description: "Start a fresh session" },
  { command: "/model", args: "", description: "Show current model info" },
  { command: "/status", args: "", description: "Show session status" },
  { command: "/skill analyze", args: "<url>", description: "Analyze a skill from URL" },
  { command: "/skill install", args: "<url>", description: "Install a skill from URL" },
  { command: "/skill confirm", args: "<token>", description: "Confirm pending skill install" },
] as const;

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

interface MessageInputProps {
  onSend: (text: string) => void;
  onCancel: () => void;
}

export function MessageInput({ onSend, onCancel }: MessageInputProps) {
  const [text, setText] = useState("");
  const [isDragging, setIsDragging] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragCountRef = useRef(0);
  const { isProcessing, isConnected, pendingAttachments, activeConversationId, addPendingAttachment, removePendingAttachment } = useChatStore();
  const [selectedCommandIdx, setSelectedCommandIdx] = useState(0);

  const filteredCommands = useMemo(() => {
    const trimmed = text.trimStart();
    if (!trimmed.startsWith("/")) return [];
    const input = trimmed.toLowerCase();
    return SLASH_COMMANDS.filter(
      (cmd) => cmd.command.startsWith(input) || `${cmd.command} ${cmd.args}`.trimEnd().startsWith(input)
    );
  }, [text]);

  const showCommands = filteredCommands.length > 0 && text.trimStart().startsWith("/");

  useEffect(() => {
    setSelectedCommandIdx(0);
  }, [filteredCommands.length]);

  const canSend = (text.trim() || pendingAttachments.length > 0) && !isProcessing && isConnected;

  const showError = useCallback((msg: string) => {
    setError(msg);
    setTimeout(() => setError(null), 3000);
  }, []);

  const processFiles = useCallback(
    async (files: FileList | File[]) => {
      const fileArray = Array.from(files);

      for (const file of fileArray) {
        if (pendingAttachments.length >= MAX_ATTACHMENTS) {
          showError(`Max ${MAX_ATTACHMENTS} files per message`);
          break;
        }
        if (file.size > MAX_FILE_SIZE) {
          showError(`File too large: ${file.name} (max 20MB)`);
          continue;
        }
        try {
          const uploaded = await uploadAttachment(file, activeConversationId ?? undefined);
          addPendingAttachment(uploaded);
        } catch {
          showError(`Failed to upload: ${file.name}`);
        }
      }
    },
    [pendingAttachments.length, activeConversationId, addPendingAttachment, showError],
  );

  const handleSend = useCallback(() => {
    const trimmed = text.trim();
    if (!canSend) return;
    onSend(trimmed);
    setText("");
    textareaRef.current?.focus();
  }, [text, canSend, onSend]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (showCommands) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelectedCommandIdx((prev) => Math.min(prev + 1, filteredCommands.length - 1));
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelectedCommandIdx((prev) => Math.max(prev - 1, 0));
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const cmd = filteredCommands[selectedCommandIdx];
        if (cmd) {
          setText(cmd.command + " ");
          setSelectedCommandIdx(0);
        }
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setText("");
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const handlePaste = useCallback(
    (e: ClipboardEvent<HTMLTextAreaElement>) => {
      const items = e.clipboardData?.items;
      if (!items) return;

      const pastedFiles: File[] = [];
      for (const item of items) {
        if (item.kind === "file") {
          const file = item.getAsFile();
          if (file) pastedFiles.push(file);
        }
      }
      if (pastedFiles.length > 0) {
        e.preventDefault();
        processFiles(pastedFiles);
      }
    },
    [processFiles],
  );

  const handleDragEnter = useCallback((e: DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    dragCountRef.current += 1;
    if (e.dataTransfer?.types.includes("Files")) {
      setIsDragging(true);
    }
  }, []);

  const handleDragLeave = useCallback((e: DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    dragCountRef.current -= 1;
    if (dragCountRef.current === 0) {
      setIsDragging(false);
    }
  }, []);

  const handleDragOver = useCallback((e: DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  }, []);

  const handleDrop = useCallback(
    (e: DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCountRef.current = 0;
      setIsDragging(false);
      const files = e.dataTransfer?.files;
      if (files && files.length > 0) {
        processFiles(files);
      }
    },
    [processFiles],
  );

  const handleFileSelect = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const files = e.target.files;
      if (files && files.length > 0) {
        processFiles(files);
      }
      e.target.value = "";
    },
    [processFiles],
  );

  return (
    <div
      className="relative border-t p-4"
      data-testid="chat-input"
      onDragEnter={handleDragEnter}
      onDragLeave={handleDragLeave}
      onDragOver={handleDragOver}
      onDrop={handleDrop}
    >
      {isDragging && (
        <div className="absolute inset-0 z-10 flex items-center justify-center rounded-b-xl border-2 border-dashed border-primary/50 bg-primary/5 backdrop-blur-[2px]">
          <div className="flex flex-col items-center gap-2 text-primary">
            <Paperclip className="h-8 w-8" />
            <span className="text-sm font-medium">Drop files here</span>
          </div>
        </div>
      )}

      {error && (
        <div className="absolute -top-10 left-4 right-4 z-20 rounded-md bg-destructive/90 px-3 py-1.5 text-xs text-destructive-foreground shadow-md">
          {error}
        </div>
      )}

      {pendingAttachments.length > 0 && (
        <div className="mb-3 flex gap-2 overflow-x-auto pb-1">
          {pendingAttachments.map((att, idx) => (
            <div
              key={`${att.file_name}-${idx}`}
              className="group relative shrink-0"
            >
              {att.kind === "image" ? (
                <img
                  src={`/api/chat/attachments/${att.id}`}
                  alt={att.file_name}
                  className="h-16 w-16 rounded-md border object-cover shadow-sm"
                />
              ) : (
                <div className="flex h-16 w-40 items-center gap-2 rounded-md border bg-muted/50 px-2 shadow-sm">
                  {att.mime_type.includes("pdf") || att.mime_type.includes("text") || att.mime_type.includes("document") ? (
                    <FileText className="h-5 w-5 shrink-0 text-muted-foreground" />
                  ) : (
                    <FileIcon className="h-5 w-5 shrink-0 text-muted-foreground" />
                  )}
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-xs font-medium">{att.file_name}</p>
                    <p className="text-[10px] text-muted-foreground">{formatFileSize(att.size)}</p>
                  </div>
                </div>
              )}
              <button
                type="button"
                onClick={() => removePendingAttachment(idx)}
                className="absolute -right-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full bg-destructive text-destructive-foreground opacity-0 shadow-sm transition-opacity group-hover:opacity-100"
                title="Remove"
              >
                <X className="h-3 w-3" />
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="flex gap-2 items-end">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFileSelect}
        />

        <Button
          variant="ghost"
          size="icon"
          className="h-9 w-9 shrink-0"
          onClick={() => fileInputRef.current?.click()}
          disabled={!isConnected || pendingAttachments.length >= MAX_ATTACHMENTS}
          title="Attach files"
        >
          <div className="relative">
            <Paperclip className="h-4 w-4" />
            {pendingAttachments.length > 0 && (
              <span className="absolute -right-2 -top-2 flex h-4 w-4 items-center justify-center rounded-full bg-primary text-[9px] font-bold text-primary-foreground">
                {pendingAttachments.length}
              </span>
            )}
          </div>
        </Button>

        {showCommands && (
          <div className="absolute bottom-full left-0 right-0 z-30 mb-1 max-h-64 overflow-y-auto rounded-lg border bg-popover p-1 shadow-lg">
            {filteredCommands.map((cmd, idx) => (
              <button
                key={cmd.command}
                type="button"
                className={cn(
                  "flex w-full items-center gap-3 rounded-md px-3 py-2 text-left text-sm transition-colors",
                  idx === selectedCommandIdx
                    ? "bg-accent text-accent-foreground"
                    : "hover:bg-accent/50"
                )}
                onMouseEnter={() => setSelectedCommandIdx(idx)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  setText(cmd.command + " ");
                  setSelectedCommandIdx(0);
                  textareaRef.current?.focus();
                }}
              >
                <code className="shrink-0 font-mono text-xs font-semibold">
                  {cmd.command}
                </code>
                {cmd.args && (
                  <span className="shrink-0 text-xs text-muted-foreground">
                    {cmd.args}
                  </span>
                )}
                <span className="text-xs text-muted-foreground ml-auto">
                  {cmd.description}
                </span>
              </button>
            ))}
          </div>
        )}

        <div className="flex-1 relative">
          <Textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value.slice(0, MAX_LENGTH))}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
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
            disabled={!canSend}
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
