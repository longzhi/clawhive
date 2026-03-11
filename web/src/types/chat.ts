// WebSocket message types for chat feature

// Attachment payload for file uploads
export interface AttachmentPayload {
  kind: string;       // "image", "document", etc.
  data: string;       // base64-encoded file data
  mime_type?: string;  // e.g., "image/png"
  file_name?: string;  // original filename
}

// Client → Server messages
export type ClientMessage =
  | { type: "send_message"; text: string; agent_id: string; conversation_id?: string; attachments?: AttachmentPayload[] }
  | { type: "cancel" }
  | { type: "ping" };

// Server → Client messages
export type ServerMessage =
  | { type: "stream_delta"; trace_id: string; delta: string; is_final: boolean }
  | { type: "tool_call_started"; trace_id: string; tool_name: string; arguments: string }
  | { type: "tool_call_completed"; trace_id: string; tool_name: string; output: string; duration_ms: number }
  | { type: "reply_ready"; trace_id: string; text: string }
  | { type: "error"; trace_id: string | null; message: string }
  | { type: "pong" };

// Conversation types for REST API
export interface Conversation {
  conversation_id: string;
  agent_id: string;
  last_message_at: string | null;
  message_count: number;
  preview: string | null;
}

export interface ChatAgent {
  agent_id: string;
  name: string | null;
  model: string | null;
}

// Message types for conversation history
export interface ChatMessage {
  role: "user" | "assistant";
  text: string;
  timestamp: string;
  tool_calls?: ToolCallInfo[];
  attachments?: AttachmentPayload[];
}

export interface ToolCallInfo {
  tool_name: string;
  arguments: string;
  output?: string;
  duration_ms?: number;
  is_running: boolean;
}
