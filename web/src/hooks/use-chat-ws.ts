import { useCallback, useEffect, useRef } from "react";
import ReconnectingWebSocket from "reconnecting-websocket";

import { useChatStore } from "@/stores/chat";
import type { AttachmentRef, ClientMessage, ServerMessage } from "@/types/chat";

export function useChatWebSocket() {
  const wsRef = useRef<ReconnectingWebSocket | null>(null);
  const {
    setConnected,
    setProcessing,
    appendStreamDelta,
    addToolCall,
    updateToolCall,
    finalizeMessage,
    addError,
  } = useChatStore();

  useEffect(() => {
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl = `${protocol}//${window.location.host}/api/chat/ws`;

    const ws = new ReconnectingWebSocket(wsUrl, [], {
      maxRetries: 10,
      reconnectionDelayGrowFactor: 1.5,
      maxReconnectionDelay: 10000,
      minReconnectionDelay: 1000,
    });

    ws.onopen = () => {
      setConnected(true);
    };

    ws.onclose = () => {
      setConnected(false);
    };

    ws.onerror = () => {
      setConnected(false);
    };

    ws.onmessage = (event) => {
      try {
        const msg: ServerMessage = JSON.parse(event.data);
        const convId = useChatStore.getState().activeConversationId;

        if (!convId) return;

        switch (msg.type) {
          case "stream_delta":
            appendStreamDelta(convId, msg.trace_id, msg.delta, msg.is_final);
            break;
          case "tool_call_started":
            addToolCall(convId, msg.trace_id, msg.tool_name, msg.arguments);
            break;
          case "tool_call_completed":
            updateToolCall(convId, msg.trace_id, msg.tool_name, msg.output, msg.duration_ms);
            break;
          case "reply_ready":
            finalizeMessage(convId, msg.trace_id, msg.text);
            break;
          case "error":
            addError(convId, msg.trace_id, msg.message);
            break;
          case "pong":
            break;
        }
      } catch (error) {
        console.error("Failed to parse WebSocket message:", error);
      }
    };

    wsRef.current = ws;

    const pingInterval = setInterval(() => {
      if (ws.readyState === WebSocket.OPEN) {
        const pingMessage: ClientMessage = { type: "ping" };
        ws.send(JSON.stringify(pingMessage));
      }
    }, 30000);

    return () => {
      clearInterval(pingInterval);
      ws.close();
      wsRef.current = null;
    };
  }, [
    addError,
    addToolCall,
    appendStreamDelta,
    finalizeMessage,
    setConnected,
    updateToolCall,
  ]);

  const sendMessage = useCallback(
    (text: string, agentId: string, conversationId?: string, attachments?: AttachmentRef[]) => {
      const ws = wsRef.current;

      if (!ws || ws.readyState !== WebSocket.OPEN) return;

      setProcessing(true);
      const msg: ClientMessage = {
        type: "send_message",
        text,
        agent_id: agentId,
        conversation_id: conversationId,
        ...(attachments && attachments.length > 0 ? { attachments } : {}),
      };
      ws.send(JSON.stringify(msg));
    },
    [setProcessing],
  );

  const cancelRequest = useCallback(() => {
    const ws = wsRef.current;

    if (!ws || ws.readyState !== WebSocket.OPEN) return;

    const msg: ClientMessage = { type: "cancel" };
    ws.send(JSON.stringify(msg));
    setProcessing(false);
  }, [setProcessing]);

  return { sendMessage, cancelRequest };
}
