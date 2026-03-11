import { useState, useRef, useCallback, useMemo, useEffect, type DragEvent, type ClipboardEvent } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Send, Square, Loader2, ImagePlus, X } from "lucide-react";
import { useChatStore } from "@/stores/chat";
import type { AttachmentPayload } from "@/types/chat";
import { cn } from "@/lib/utils";

const MAX_LENGTH = 10000;
const MAX_ATTACHMENTS = 5;
const MAX_FILE_SIZE = 7.5 * 1024 * 1024; // ~7.5MB raw → ~10MB base64
const ACCEPTED_TYPES = ["image/jpeg", "image/png", "image/gif", "image/webp"];

const SLASH_COMMANDS = [
  { command: "/new", args: "[model]", description: "Start a fresh session" },
  { command: "/model", args: "", description: "Show current model info" },
  { command: "/status", args: "", description: "Show session status" },
  { command: "/skill analyze", args: "<url>", description: "Analyze a skill from URL" },
  { command: "/skill install", args: "<url>", description: "Install a skill from URL" },
  { command: "/skill confirm", args: "<token>", description: "Confirm pending skill install" },
] as const;

function fileToBase64(file: File): Promise<{ data: string; mime_type: string }> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result as string;
      const base64 = result.split(",")[1]; // strip data:...;base64, prefix
      resolve({ data: base64, mime_type: file.type });
    };
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
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
  const { isProcessing, isConnected, pendingAttachments, addPendingAttachment, removePendingAttachment } = useChatStore();
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
      const current = pendingAttachments.length;
      const fileArray = Array.from(files);

      for (const file of fileArray) {
        if (current + pendingAttachments.length >= MAX_ATTACHMENTS) {
          showError(`Max ${MAX_ATTACHMENTS} images per message`);
          break;
        }
        if (!ACCEPTED_TYPES.includes(file.type)) {
          showError(`Unsupported type: ${file.type}. Use JPEG, PNG, GIF, or WebP.`);
          continue;
        }
        if (file.size > MAX_FILE_SIZE) {
          showError(`File too large: ${file.name} (max ~7.5MB)`);
          continue;
        }
        try {
          const { data, mime_type } = await fileToBase64(file);
          const attachment: AttachmentPayload = {
            kind: "image",
            data,
            mime_type,
            file_name: file.name,
          };
          addPendingAttachment(attachment);
        } catch {
          showError(`Failed to read: ${file.name}`);
        }
      }
    },
    [pendingAttachments.length, addPendingAttachment, showError],
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

  // --- Paste handler ---
  const handlePaste = useCallback(
    (e: ClipboardEvent<HTMLTextAreaElement>) => {
      const items = e.clipboardData?.items;
      if (!items) return;

      const imageFiles: File[] = [];
      for (const item of items) {
        if (item.type.startsWith("image/")) {
          const file = item.getAsFile();
          if (file) imageFiles.push(file);
        }
      }
      if (imageFiles.length > 0) {
        e.preventDefault();
        processFiles(imageFiles);
      }
    },
    [processFiles],
  );

  // --- Drag and drop ---
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

  // --- File picker ---
  const handleFileSelect = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const files = e.target.files;
      if (files && files.length > 0) {
        processFiles(files);
      }
      // Reset so same file can be re-selected
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
      {/* Drag overlay */}
      {isDragging && (
        <div className="absolute inset-0 z-10 flex items-center justify-center rounded-b-xl border-2 border-dashed border-primary/50 bg-primary/5 backdrop-blur-[2px]">
          <div className="flex flex-col items-center gap-2 text-primary">
            <ImagePlus className="h-8 w-8" />
            <span className="text-sm font-medium">Drop images here</span>
          </div>
        </div>
      )}

      {/* Error toast */}
      {error && (
        <div className="absolute -top-10 left-4 right-4 z-20 rounded-md bg-destructive/90 px-3 py-1.5 text-xs text-destructive-foreground shadow-md">
          {error}
        </div>
      )}

      {/* Pending attachment previews */}
      {pendingAttachments.length > 0 && (
        <div className="mb-3 flex gap-2 overflow-x-auto pb-1">
          {pendingAttachments.map((att, idx) => (
            <div
              key={`${att.file_name ?? "img"}-${idx}`}
              className="group relative shrink-0"
            >
              <img
                src={`data:${att.mime_type};base64,${att.data}`}
                alt={att.file_name ?? "attachment"}
                className="h-16 w-16 rounded-md border object-cover shadow-sm"
              />
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
        {/* Hidden file input */}
        <input
          ref={fileInputRef}
          type="file"
          accept={ACCEPTED_TYPES.join(",")}
          multiple
          className="hidden"
          onChange={handleFileSelect}
        />

        {/* Attach button */}
        <Button
          variant="ghost"
          size="icon"
          className="h-9 w-9 shrink-0"
          onClick={() => fileInputRef.current?.click()}
          disabled={!isConnected || pendingAttachments.length >= MAX_ATTACHMENTS}
          title="Attach images"
        >
          <div className="relative">
            <ImagePlus className="h-4 w-4" />
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
