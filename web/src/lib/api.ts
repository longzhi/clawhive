import type { UploadedAttachment } from "@/types/chat";

export async function uploadAttachment(file: File, conversationId?: string): Promise<UploadedAttachment> {
  const formData = new FormData();
  formData.append("file", file);
  if (conversationId) formData.append("conversation_id", conversationId);

  const res = await fetch("/api/chat/attachments", {
    method: "POST",
    body: formData,
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.error || `Upload failed: ${res.status}`);
  }
  return res.json();
}

export async function apiFetch<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...options,
    headers: { 'Content-Type': 'application/json', ...options?.headers },
  });
  if (!res.ok) {
    let errorMessage = `API error: ${res.status}`;
    try {
      const body = await res.text();
      if (body) {
        try {
          const json = JSON.parse(body);
          errorMessage = json.error || json.message || body || errorMessage;
        } catch {
          errorMessage = body || errorMessage;
        }
      }
    } catch {
      // If reading body fails, use default error message
    }
    throw new Error(errorMessage);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (!text) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new Error(`Unexpected response from ${path}`);
  }
}
