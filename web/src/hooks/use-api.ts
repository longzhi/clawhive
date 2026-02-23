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

export interface AuthProfileItem {
  name: string;
  provider: string;
  kind: string;
  active: boolean;
}

export interface AuthStatus {
  active_profile: string | null;
  profiles: AuthProfileItem[];
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

export interface ConnectorConfig {
  connector_id: string;
  token: string;
}

export interface ChannelConfig {
  enabled: boolean;
  connectors: ConnectorConfig[];
}

export type ChannelsResponse = Record<string, ChannelConfig>;

export interface ConnectorStatus {
  kind: string;
  connector_id: string;
  status: "connected" | "error" | "inactive";
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

export function useAuthStatus() {
  return useQuery({ queryKey: ["auth-status"], queryFn: () => apiFetch<AuthStatus>("/api/auth/status") });
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
  return useQuery({ queryKey: ["channels"], queryFn: () => apiFetch<ChannelsResponse>("/api/channels") });
}

export function useChannelStatus() {
  return useQuery({
    queryKey: ["channel-status"],
    queryFn: () => apiFetch<ConnectorStatus[]>("/api/channels/status"),
    refetchInterval: 5000,
  });
}

export function useRouting() {
  return useQuery({ queryKey: ["routing"], queryFn: () => apiFetch<Record<string, unknown>>("/api/routing") });
}

export function useUpdateChannels() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: ChannelsResponse) => apiFetch<ChannelsResponse>("/api/channels", { method: "PUT", body: JSON.stringify(data) }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["channels"] }),
  });
}

export function useAddConnector() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kind, connectorId, token }: { kind: string; connectorId: string; token: string }) =>
      apiFetch(`/api/channels/${kind}/connectors`, {
        method: "POST",
        body: JSON.stringify({ connector_id: connectorId, token }),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["channels"] });
      qc.invalidateQueries({ queryKey: ["channel-status"] });
    },
  });
}

export function useRemoveConnector() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kind, connectorId }: { kind: string; connectorId: string }) =>
      apiFetch(`/api/channels/${kind}/connectors/${connectorId}`, { method: "DELETE" }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["channels"] });
      qc.invalidateQueries({ queryKey: ["channel-status"] });
    },
  });
}

export function useUpdateRouting() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: Record<string, unknown>) =>
      apiFetch<Record<string, unknown>>("/api/routing", { method: "PUT", body: JSON.stringify(data) }),
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
