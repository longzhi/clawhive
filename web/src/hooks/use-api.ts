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
  key_configured: boolean;
  auth_profile?: string | null;
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

export interface OpenAiOAuthStartResponse {
  flow_id: string;
  authorize_url: string;
  profile_name: string;
  replaces_existing: boolean;
}

export interface OpenAiOAuthCompleteRequest {
  flow_id: string;
  callback_input?: string;
}

export interface OpenAiOAuthCompleteResponse {
  profile_name: string;
  chatgpt_account_id?: string | null;
}

export interface OpenAiOAuthFlowStatus {
  flow_id: string;
  callback_listener_active: boolean;
  callback_captured: boolean;
  message?: string | null;
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
  channels_total: number;
}

export interface WebSearchConfig {
  enabled: boolean;
  provider: string | null;
  api_key: string | null;
  has_api_key?: boolean;
}

export interface ActionbookConfig {
  enabled: boolean;
  installed: boolean;
}

export interface ConnectorConfig {
  connector_id: string;
  token?: string;
  // Feishu
  app_id?: string;
  app_secret?: string;
  // DingTalk
  client_id?: string;
  client_secret?: string;
  // WeCom
  bot_id?: string;
  secret?: string;
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
export interface ScheduleListItem {
  schedule_id: string;
  name: string;
  description: string | null;
  enabled: boolean;
  schedule: {
    kind: "cron" | "at" | "every";
    expr?: string;
    tz?: string;
    at?: string;
    interval_ms?: number;
    anchor_ms?: number;
  };
  agent_id: string;
  session_mode: "main" | "isolated";
  next_run_at: string | null;
  last_run_status: "ok" | "error" | "skipped" | null;
  last_run_at: string | null;
  consecutive_errors: number;
}
export interface ScheduleRunHistoryItem {
  started_at: string;
  ended_at: string;
  status: "ok" | "error" | "skipped";
  error: string | null;
  duration_ms: number;
  response: string | null;
  session_key: string | null;
}

// Setup types
export interface SetupStatus {
  needs_setup: boolean;
  has_providers: boolean;
  has_active_agents: boolean;
  has_channels: boolean;
}

export interface CreateProviderRequest {
  provider_id: string;
  api_base: string;
  api_key?: string;
  auth_profile?: string;
  models: string[];
}

export interface CreateAgentRequest {
  agent_id: string;
  name: string;
  emoji: string;
  primary_model: string;
  thinking_level?: string;
}

export interface ListModelsRequest {
  provider_type: string;
  api_key?: string;
  base_url?: string;
}

export interface ModelInfoResponse {
  id: string;
  context_window?: number;
  max_output_tokens?: number;
  reasoning: boolean;
  vision: boolean;
}

// Setup hooks
export function useSetupStatus() {
  return useQuery({
    queryKey: ["setup-status"],
    queryFn: () => apiFetch<SetupStatus>("/api/setup/status"),
  });
}

export function useCreateProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: CreateProviderRequest) =>
      apiFetch<{ provider_id: string; enabled: boolean }>("/api/providers", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["providers"] });
      qc.invalidateQueries({ queryKey: ["setup-status"] });
    },
  });
}

export function useListModels() {
  return useMutation({
    mutationFn: (data: ListModelsRequest) =>
      apiFetch<{ models: ModelInfoResponse[] }>("/api/setup/list-models", {
        method: "POST",
        body: JSON.stringify(data),
      }),
  });
}

export function useCreateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: CreateAgentRequest) =>
      apiFetch<{ agent_id: string; enabled: boolean }>("/api/agents", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
      qc.invalidateQueries({ queryKey: ["setup-status"] });
    },
  });
}

export function useRestart() {
  return useMutation({
    mutationFn: () =>
      apiFetch<{ ok: boolean }>("/api/setup/restart", { method: "POST" }),
  });
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

export function useStartOpenAiOAuth() {
  return useMutation({
    mutationFn: () =>
      apiFetch<OpenAiOAuthStartResponse>("/api/auth/openai/start", {
        method: "POST",
        body: JSON.stringify({}),
      }),
  });
}

export function useCompleteOpenAiOAuth() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: OpenAiOAuthCompleteRequest) =>
      apiFetch<OpenAiOAuthCompleteResponse>("/api/auth/openai/complete", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["auth-status"] });
    },
  });
}

export function useOpenAiOAuthFlowStatus(flowId: string | null) {
  return useQuery({
    queryKey: ["openai-oauth-flow", flowId],
    queryFn: () => apiFetch<OpenAiOAuthFlowStatus>(`/api/auth/openai/flow/${flowId}`),
    enabled: Boolean(flowId),
    refetchInterval: (query) => {
      const data = query.state.data as OpenAiOAuthFlowStatus | undefined;
      if (!flowId) return false;
      if (data?.callback_captured) return false;
      return 1000;
    },
  });
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

export function useRouting(enabled = true) {
  return useQuery({
    queryKey: ["routing"],
    queryFn: () => apiFetch<Record<string, unknown>>("/api/routing"),
    enabled,
    retry: false,
  });
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
    mutationFn: ({ kind, connectorId, token, groups, requireMention, appId, appSecret, clientId, clientSecret, botId, secret }: {
      kind: string;
      connectorId: string;
      token?: string;
      groups?: string[];
      requireMention?: boolean;
      appId?: string;
      appSecret?: string;
      clientId?: string;
      clientSecret?: string;
      botId?: string;
      secret?: string;
    }) =>
      apiFetch(`/api/channels/${kind}/connectors`, {
        method: "POST",
        body: JSON.stringify({
          connector_id: connectorId,
          ...(token ? { token } : {}),
          ...(appId ? { app_id: appId } : {}),
          ...(appSecret ? { app_secret: appSecret } : {}),
          ...(clientId ? { client_id: clientId } : {}),
          ...(clientSecret ? { client_secret: clientSecret } : {}),
          ...(botId ? { bot_id: botId } : {}),
          ...(secret ? { secret } : {}),
          ...(groups && groups.length > 0 ? { groups } : {}),
          ...(requireMention !== undefined ? { require_mention: requireMention } : {}),
        }),
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

export function useSchedules() {
  return useQuery({
    queryKey: ["schedules"],
    queryFn: () => apiFetch<ScheduleListItem[]>("/api/schedules"),
    refetchInterval: 10_000,
  });
}

export function useRunSchedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (scheduleId: string) =>
      apiFetch<void>(`/api/schedules/${scheduleId}/run`, { method: "POST" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["schedules"] }),
  });
}

export function useToggleSchedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      apiFetch<void>(`/api/schedules/${id}`, {
        method: "PATCH",
        body: JSON.stringify({ enabled }),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["schedules"] }),
  });
}

export function useScheduleHistory(scheduleId: string, limit = 10) {
  return useQuery({
    queryKey: ["schedules", scheduleId, "history", limit],
    queryFn: () => apiFetch<ScheduleRunHistoryItem[]>(`/api/schedules/${scheduleId}/history?limit=${limit}`),
    enabled: !!scheduleId,
  });
}

export interface ScheduleDetail {
  schedule_id: string;
  name: string;
  description: string | null;
  enabled: boolean;
  schedule: {
    kind: "cron" | "at" | "every";
    expr?: string;
    tz?: string;
    at?: string;
    interval_ms?: number;
    anchor_ms?: number;
  };
  agent_id: string;
  session_mode: "main" | "isolated";
  payload: {
    kind: "system_event" | "agent_turn" | "direct_deliver";
    text?: string;
    message?: string;
    model?: string | null;
    thinking?: string | null;
    timeout_seconds?: number;
    light_context?: boolean;
  } | null;
  timeout_seconds: number;
  delete_after_run: boolean;
  delivery: {
    mode: "none" | "announce" | "webhook";
    channel?: string | null;
    connector_id?: string | null;
    source_channel_type?: string | null;
    source_connector_id?: string | null;
    source_conversation_scope?: string | null;
    source_user_scope?: string | null;
    webhook_url?: string | null;
    best_effort: boolean;
    failure_destination?: { channel?: string; connector_id?: string; conversation_scope?: string } | null;
  };
}

export function useScheduleDetail(scheduleId: string) {
  return useQuery({
    queryKey: ["schedules", scheduleId, "detail"],
    queryFn: () => apiFetch<ScheduleDetail>(`/api/schedules/${scheduleId}/detail`),
    enabled: !!scheduleId,
  });
}

export function useUpdateSchedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: ScheduleDetail }) =>
      apiFetch<void>(`/api/schedules/${id}`, {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["schedules"] });
    },
  });
}

export function useWebSearchConfig() {
  return useQuery({
    queryKey: ["web-search-config"],
    queryFn: () => apiFetch<WebSearchConfig>("/api/setup/tools/web-search"),
  });
}

export function useActionbookConfig() {
  return useQuery({
    queryKey: ["actionbook-config"],
    queryFn: () => apiFetch<ActionbookConfig>("/api/setup/tools/actionbook"),
  });
}

export interface ModelPresetInfo {
  id: string;
  context_window: number;
  max_output_tokens: number;
  reasoning: boolean;
  vision: boolean;
}

export interface ProviderPreset {
  id: string;
  name: string;
  api_base: string;
  needs_key: boolean;
  needs_base_url: boolean;
  default_model: string;
  models: ModelPresetInfo[];
}

export function useProviderPresets() {
  return useQuery({
    queryKey: ["provider-presets"],
    queryFn: () => apiFetch<ProviderPreset[]>("/api/setup/provider-presets"),
    staleTime: Infinity,
  });
}

export function useUpdateWebSearch() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: WebSearchConfig) =>
      apiFetch<WebSearchConfig>("/api/setup/tools/web-search", {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["web-search-config"] }),
  });
}

export function useUpdateActionbook() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: { enabled: boolean }) =>
      apiFetch<ActionbookConfig>("/api/setup/tools/actionbook", {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["actionbook-config"] }),
  });
}

export function useUpdateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: AgentDetail }) =>
      apiFetch<AgentDetail>(`/api/agents/${id}`, {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["agents"] }),
  });
}

// Skills types
export interface InstalledSkillSummary {
  name: string;
  description: string;
  has_permissions: boolean;
  path: string;
}

export interface AnalyzeFinding {
  severity: string;
  file: string;
  line: number;
  pattern: string;
  reason: string;
}

export interface AnalyzeSkillResponse {
  skill_name: string;
  description: string;
  findings: AnalyzeFinding[];
  has_high_risk: boolean;
  rendered_report: string;
}

export interface InstallSkillResponse {
  skill_name: string;
  target_path: string;
  findings_count: number;
  high_risk: boolean;
}

// Skills hooks
export function useSkills() {
  return useQuery({
    queryKey: ["skills"],
    queryFn: () => apiFetch<InstalledSkillSummary[]>("/api/skills"),
  });
}

export function useAnalyzeSkill() {
  return useMutation({
    mutationFn: (source: string) =>
      apiFetch<AnalyzeSkillResponse>("/api/skills/analyze", {
        method: "POST",
        body: JSON.stringify({ source }),
      }),
  });
}

export function useCreateSchedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: Record<string, unknown>) =>
      apiFetch<Record<string, unknown>>("/api/schedules", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["schedules"] }),
  });
}

export function useDeleteSchedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) =>
      apiFetch<void>(`/api/schedules/${id}`, { method: "DELETE" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["schedules"] }),
  });
}

export function useInstallSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ source, allowHighRisk }: { source: string; allowHighRisk: boolean }) =>
      apiFetch<InstallSkillResponse>("/api/skills/install", {
        method: "POST",
        body: JSON.stringify({ source, allow_high_risk: allowHighRisk }),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["skills"] }),
  });
}


export function useUpdateProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: Record<string, unknown> }) =>
      apiFetch<Record<string, unknown>>(`/api/providers/${id}`, {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["providers"] }),
  });
}

export function useDeleteProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) =>
      apiFetch<void>(`/api/providers/${id}`, { method: "DELETE" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["providers"] }),
  });
}

// Auth hooks
export function useAuthCheck() {
  return useQuery({
    queryKey: ["auth-check"],
    queryFn: () => apiFetch<{ authenticated: boolean; auth_required: boolean }>("/api/auth/check"),
    retry: false,
  });
}

export function useLogin() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (password: string) =>
      apiFetch<void>("/api/auth/login", {
        method: "POST",
        body: JSON.stringify({ password }),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["auth-check"] }),
  });
}

export function useLogout() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => apiFetch<void>("/api/auth/logout", { method: "POST" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["auth-check"] }),
  });
}

export function useSetPassword() {
  return useMutation({
    mutationFn: (password: string) =>
      apiFetch<void>("/api/auth/set-password", {
        method: "POST",
        body: JSON.stringify({ password }),
      }),
  });
}

// Chat hooks
export function useChatConversations() {
  return useQuery({
    queryKey: ["chat-conversations"],
    queryFn: () => apiFetch<Array<{ conversation_id: string; agent_id: string; last_message_at: string | null; message_count: number; preview: string | null }>>("/api/chat/conversations"),
  });
}

export function useChatAgents() {
  return useQuery({
    queryKey: ["chat-agents"],
    queryFn: () => apiFetch<Array<{ agent_id: string; name: string | null; model: string | null }>>("/api/chat/agents"),
  });
}

export function useCreateChatConversation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: { agent_id: string }) =>
      apiFetch<{ conversation_id: string; agent_id: string }>("/api/chat/conversations", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["chat-conversations"] });
    },
  });
}

export function useDeleteChatConversation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) =>
      apiFetch<void>(`/api/chat/conversations/${id}`, { method: "DELETE" }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["chat-conversations"] });
    },
  });
}

export function useChatMessages(conversationId: string | null) {
  return useQuery({
    queryKey: ["chat-messages", conversationId],
    queryFn: () => apiFetch<Array<{
      role: string;
      text: string;
      timestamp: string;
      tool_calls?: Array<{
        tool_name: string;
        arguments: string;
        output?: string;
        duration_ms?: number;
        is_running: boolean;
      }>;
    }>>(`/api/chat/conversations/${conversationId}/messages`),
    enabled: !!conversationId,
  });
}
