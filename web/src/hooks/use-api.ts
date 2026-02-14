"use client";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { apiFetch } from "@/lib/api";

// Types matching backend responses
export interface AgentSummary {
  agent_id: string;
  enabled: boolean;
  name: string;
  emoji: string;
  primary_model: string;
  tools: string[];
}

export interface AgentDetail {
  agent_id: string;
  enabled: boolean;
  identity: { name: string; emoji: string };
  model_policy: { primary: string; fallbacks: string[] };
  tool_policy: { allow: string[] };
  memory_policy: { mode: string; write_scope: string };
  sub_agent?: { allow_spawn: boolean };
}

export interface ProviderSummary {
  provider_id: string;
  enabled: boolean;
  api_base: string;
  api_key_env: string;
  key_configured: boolean;
  models: string[];
}

export interface SessionSummary {
  session_key: string;
  file_name: string;
  message_count: number;
  last_modified: string;
}

export interface SessionMessage {
  role: string;
  text: string;
  timestamp: string;
}

export interface Metrics {
  agents_active: number;
  agents_total: number;
  sessions_total: number;
  providers_total: number;
}

// Hooks
export function useAgents() {
  return useQuery({ queryKey: ["agents"], queryFn: () => apiFetch<AgentSummary[]>("/api/agents") });
}

export function useAgent(id: string) {
  return useQuery({ queryKey: ["agents", id], queryFn: () => apiFetch<AgentDetail>(`/api/agents/${id}`), enabled: !!id });
}

export function useToggleAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => apiFetch<{agent_id: string; enabled: boolean}>(`/api/agents/${id}/toggle`, { method: "POST" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["agents"] }),
  });
}

export function useProviders() {
  return useQuery({ queryKey: ["providers"], queryFn: () => apiFetch<ProviderSummary[]>("/api/providers") });
}

export function useTestProvider() {
  return useMutation({
    mutationFn: (id: string) => apiFetch<{ok: boolean; message: string}>(`/api/providers/${id}/test`, { method: "POST" }),
  });
}

export function useSetProviderKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, apiKey }: { id: string; apiKey: string }) =>
      apiFetch<{ ok: boolean; provider_id: string }>(`/api/providers/${id}/key`, {
        method: "POST",
        body: JSON.stringify({ api_key: apiKey }),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["providers"] }),
  });
}

export function useChannels() {
  return useQuery({ queryKey: ["channels"], queryFn: () => apiFetch<Record<string, any>>("/api/channels") });
}

export function useRouting() {
  return useQuery({ queryKey: ["routing"], queryFn: () => apiFetch<Record<string, any>>("/api/routing") });
}

export function useUpdateChannels() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: Record<string, any>) => apiFetch<Record<string, any>>("/api/channels", { method: "PUT", body: JSON.stringify(data) }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["channels"] }),
  });
}

export function useUpdateRouting() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: any) => apiFetch<any>("/api/routing", { method: "PUT", body: JSON.stringify(data) }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["routing"] }),
  });
}

export function useSessions() {
  return useQuery({ queryKey: ["sessions"], queryFn: () => apiFetch<SessionSummary[]>("/api/sessions") });
}

export function useSessionMessages(key: string) {
  return useQuery({ queryKey: ["sessions", key], queryFn: () => apiFetch<SessionMessage[]>(`/api/sessions/${key}/messages`), enabled: !!key });
}

export function useMetrics() {
  return useQuery({ queryKey: ["metrics"], queryFn: () => apiFetch<Metrics>("/api/events/metrics"), refetchInterval: 10000 });
}
