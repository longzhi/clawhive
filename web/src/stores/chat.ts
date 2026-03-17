import { create } from "zustand";

import type { UploadedAttachment } from "@/types/chat";

export interface ToolCallInfo {
  tool_name: string;
  arguments: string;
  output?: string;
  duration_ms?: number;
  is_running: boolean;
}

export interface ChatMessageItem {
  id: string;
  role: "user" | "assistant";
  text: string;
  timestamp: string;
  tool_calls: ToolCallInfo[];
  is_streaming: boolean;
  attachments?: UploadedAttachment[];
}

export interface ConversationItem {
  id: string;
  agent_id: string;
  title: string;
  last_message_at: string | null;
  messages: ChatMessageItem[];
}

interface ChatState {
  conversations: ConversationItem[];
  activeConversationId: string | null;
  isConnected: boolean;
  isProcessing: boolean;
  selectedAgentId: string | null;
  pendingAttachments: UploadedAttachment[];

  // Actions
  setConversations: (conversations: ConversationItem[]) => void;
  setActiveConversation: (id: string | null) => void;
  setConnected: (connected: boolean) => void;
  setProcessing: (processing: boolean) => void;
  setSelectedAgent: (agentId: string | null) => void;

  // Message actions
  addMessage: (conversationId: string, message: ChatMessageItem) => void;
  appendStreamDelta: (conversationId: string, traceId: string, delta: string, isFinal: boolean) => void;
  addToolCall: (conversationId: string, traceId: string, toolName: string, args: string) => void;
  updateToolCall: (conversationId: string, traceId: string, toolName: string, output: string, durationMs: number) => void;
  finalizeMessage: (conversationId: string, traceId: string, text: string) => void;
  addError: (conversationId: string, traceId: string | null, message: string) => void;

  // Attachment actions
  addPendingAttachment: (attachment: UploadedAttachment) => void;
  removePendingAttachment: (index: number) => void;
  clearPendingAttachments: () => void;
  hydrateMessages: (conversationId: string, messages: ChatMessageItem[]) => void;
}

export const useChatStore = create<ChatState>((set) => ({
  conversations: [],
  activeConversationId: null,
  isConnected: false,
  isProcessing: false,
  selectedAgentId: null,
  pendingAttachments: [],

  setConversations: (conversations) => set({ conversations }),
  setActiveConversation: (id) => set({ activeConversationId: id }),
  setConnected: (connected) => set({ isConnected: connected }),
  setProcessing: (processing) => set({ isProcessing: processing }),
  setSelectedAgent: (agentId) => set({ selectedAgentId: agentId }),

  addPendingAttachment: (attachment) =>
    set((state) => ({
      pendingAttachments: [...state.pendingAttachments, attachment],
    })),

  removePendingAttachment: (index) =>
    set((state) => ({
      pendingAttachments: state.pendingAttachments.filter((_, i) => i !== index),
    })),

  clearPendingAttachments: () => set({ pendingAttachments: [] }),

  hydrateMessages: (conversationId, messages) =>
    set((state) => ({
      conversations: state.conversations.map((c) =>
        c.id === conversationId && c.messages.length === 0
          ? { ...c, messages }
          : c
      ),
    })),


  addMessage: (conversationId, message) =>
    set((state) => ({
      conversations: state.conversations.map((c) =>
        c.id === conversationId ? { ...c, messages: [...c.messages, message] } : c
      ),
    })),

  appendStreamDelta: (conversationId, traceId, delta, isFinal) =>
    set((state) => ({
      conversations: state.conversations.map((c) => {
        if (c.id !== conversationId) return c;
        const messages = [...c.messages];
        const lastMsg = messages[messages.length - 1];
        if (lastMsg && lastMsg.role === "assistant" && lastMsg.is_streaming) {
          messages[messages.length - 1] = {
            ...lastMsg,
            text: lastMsg.text + delta,
            is_streaming: !isFinal,
          };
        } else {
          messages.push({
            id: traceId,
            role: "assistant",
            text: delta,
            timestamp: new Date().toISOString(),
            tool_calls: [],
            is_streaming: !isFinal,
          });
        }
        return { ...c, messages };
      }),
    })),

  addToolCall: (conversationId, traceId, toolName, args) =>
    set((state) => ({
      conversations: state.conversations.map((c) => {
        if (c.id !== conversationId) return c;
        const messages = [...c.messages];
        const lastMsg = messages[messages.length - 1];
        if (lastMsg && lastMsg.role === "assistant") {
          messages[messages.length - 1] = {
            ...lastMsg,
            tool_calls: [
              ...lastMsg.tool_calls,
              { tool_name: toolName, arguments: args, is_running: true },
            ],
          };
        } else {
          // Tool call arrived before any streaming — create assistant message
          messages.push({
            id: traceId,
            role: "assistant",
            text: "",
            timestamp: new Date().toISOString(),
            tool_calls: [{ tool_name: toolName, arguments: args, is_running: true }],
            is_streaming: true,
          });
        }
        return { ...c, messages };
      }),
    })),

  updateToolCall: (conversationId, traceId, toolName, output, durationMs) =>
    set((state) => ({
      conversations: state.conversations.map((c) => {
        if (c.id !== conversationId) return c;
        const messages = [...c.messages];
        const lastMsg = messages[messages.length - 1];
        if (lastMsg && lastMsg.role === "assistant") {
          messages[messages.length - 1] = {
            ...lastMsg,
            tool_calls: lastMsg.tool_calls.map((tc) =>
              tc.tool_name === toolName && tc.is_running
                ? { ...tc, output, duration_ms: durationMs, is_running: false }
                : tc
            ),
          };
        }
        return { ...c, messages };
      }),
    })),

  finalizeMessage: (conversationId, traceId, text) =>
    set((state) => ({
      conversations: state.conversations.map((c) => {
        if (c.id !== conversationId) return c;
        const messages = [...c.messages];
        const lastMsg = messages[messages.length - 1];
        if (lastMsg && lastMsg.role === "assistant") {
          messages[messages.length - 1] = {
            ...lastMsg,
            text,
            is_streaming: false,
          };
        } else {
          // No streaming happened — create new assistant message
          messages.push({
            id: traceId,
            role: "assistant",
            text,
            timestamp: new Date().toISOString(),
            tool_calls: [],
            is_streaming: false,
          });
        }
        return { ...c, messages };
      }),
      isProcessing: false,
    })),

  addError: (conversationId, traceId, message) =>
    set((state) => ({
      conversations: state.conversations.map((c) => {
        if (c.id !== conversationId) return c;
        return {
          ...c,
          messages: [
            ...c.messages,
            {
              id: traceId ?? `error-${Date.now()}`,
              role: "assistant" as const,
              text: `Error: ${message}`,
              timestamp: new Date().toISOString(),
              tool_calls: [],
              is_streaming: false,
            },
          ],
        };
      }),
      isProcessing: false,
    })),
}));
