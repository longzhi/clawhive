import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  useAuthStatus,
  useCompleteOpenAiOAuth,
  useOpenAiOAuthFlowStatus,
  useStartOpenAiOAuth,
  type OpenAiOAuthStartResponse,
} from "@/hooks/use-api";
import { CheckCircle2, Copy, ExternalLink, Loader2, RefreshCw } from "lucide-react";
import { toast } from "sonner";

export function OpenAiOAuthSetup() {
  const { data: authStatus } = useAuthStatus();
  const startOpenAiOAuth = useStartOpenAiOAuth();
  const completeOpenAiOAuth = useCompleteOpenAiOAuth();
  const [flow, setFlow] = useState<OpenAiOAuthStartResponse | null>(null);
  const [callbackInput, setCallbackInput] = useState("");
  const [autoCompleteFlowId, setAutoCompleteFlowId] = useState<string | null>(null);
  const { data: flowStatus } = useOpenAiOAuthFlowStatus(flow?.flow_id ?? null);

  const profileName =
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth" && profile.active)?.name ??
    authStatus?.profiles.find((profile) => profile.kind === "OpenAiOAuth")?.name ??
    "openai-oauth";
  const hasOpenAiOAuth =
    authStatus?.profiles.some((profile) => profile.kind === "OpenAiOAuth") ?? false;
  const statusLabel = hasOpenAiOAuth ? "Saved Login" : "Missing";
  const helperText = hasOpenAiOAuth
    ? "A ChatGPT OAuth login is already stored. You can save the provider now or reconnect to replace it."
    : "Sign in from the web UI. No CLI command is required.";

  useEffect(() => {
    if (hasOpenAiOAuth) {
      setFlow(null);
      setCallbackInput("");
      setAutoCompleteFlowId(null);
    }
  }, [hasOpenAiOAuth]);

  useEffect(() => {
    if (
      !flow ||
      !flowStatus?.callback_captured ||
      completeOpenAiOAuth.isPending ||
      autoCompleteFlowId === flow.flow_id
    ) {
      return;
    }

    setAutoCompleteFlowId(flow.flow_id);
    completeOpenAiOAuth
      .mutateAsync({
        flow_id: flow.flow_id,
      })
      .then(() => {
        setFlow(null);
        setCallbackInput("");
        toast.success("ChatGPT OAuth connected");
      })
      .catch((err) => {
        setAutoCompleteFlowId(null);
        toast.error(err instanceof Error ? err.message : "Failed to complete OpenAI OAuth");
      });
  }, [
    autoCompleteFlowId,
    completeOpenAiOAuth,
    flow,
    flowStatus?.callback_captured,
  ]);

  const handleStart = async () => {
    const popup = typeof window !== "undefined"
      ? window.open("", "_blank")
      : null;

    try {
      if (popup && !popup.closed) {
        popup.document.write(`
          <!doctype html>
          <html>
            <head>
              <meta charset="utf-8" />
              <title>Clawhive OAuth</title>
              <style>
                body {
                  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
                  margin: 0;
                  min-height: 100vh;
                  display: grid;
                  place-items: center;
                  background: #111827;
                  color: #f9fafb;
                }
                .card {
                  max-width: 32rem;
                  padding: 1.5rem;
                  border: 1px solid rgba(255,255,255,0.12);
                  border-radius: 0.75rem;
                  background: rgba(17,24,39,0.92);
                }
                .muted {
                  color: #9ca3af;
                  font-size: 0.95rem;
                }
              </style>
            </head>
            <body>
              <div class="card">
                <h1>Opening ChatGPT sign-in…</h1>
                <p class="muted">If nothing happens, return to Clawhive and use the copied login URL.</p>
              </div>
            </body>
          </html>
        `);
        popup.document.close();
      }

      const result = await startOpenAiOAuth.mutateAsync();
      setFlow(result);
      setAutoCompleteFlowId(null);

      if (popup && !popup.closed) {
        popup.location.replace(result.authorize_url);
      } else {
        const opened = window.open(result.authorize_url, "_blank");
        if (!opened) {
          toast.message("Popup blocked. Use the copied login URL below.");
        }
      }

      toast.success(
        result.replaces_existing
          ? `OpenAI OAuth will replace ${result.profile_name}`
          : "OpenAI OAuth flow started",
      );
    } catch (err) {
      popup?.close();
      toast.error(err instanceof Error ? err.message : "Failed to start OpenAI OAuth");
    }
  };

  const handleComplete = async () => {
    if (!flow) return;
    try {
      await completeOpenAiOAuth.mutateAsync({
        flow_id: flow.flow_id,
        callback_input: callbackInput.trim() || undefined,
      });
      setFlow(null);
      setCallbackInput("");
      toast.success("ChatGPT OAuth connected");
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Failed to complete OpenAI OAuth");
    }
  };

  const copyLoginUrl = async () => {
    if (!flow) return;
    try {
      await navigator.clipboard.writeText(flow.authorize_url);
      toast.success("Login URL copied");
    } catch {
      toast.error("Failed to copy login URL");
    }
  };

  return (
    <div className="rounded-lg border border-dashed border-primary/20 bg-background/80 p-3">
      <div className="flex items-center justify-between gap-3">
        <div>
          <p className="text-sm font-medium">ChatGPT OAuth</p>
          <p className="text-xs text-muted-foreground">
            {helperText}
          </p>
        </div>
        <Badge variant={hasOpenAiOAuth ? "default" : "secondary"}>
          {statusLabel}
        </Badge>
      </div>

      <p className="mt-2 text-xs text-muted-foreground">
        The login will be stored as{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono">{profileName}</code>
        {" "}and reused by the{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono">openai-chatgpt</code>
        {" "}provider.
      </p>

      <div className="mt-3 flex flex-wrap gap-2">
        <Button
          type="button"
          size="sm"
          onClick={handleStart}
          disabled={startOpenAiOAuth.isPending}
        >
          {startOpenAiOAuth.isPending ? (
            <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" />
          ) : hasOpenAiOAuth ? (
            <RefreshCw className="mr-2 h-3.5 w-3.5" />
          ) : (
            <ExternalLink className="mr-2 h-3.5 w-3.5" />
          )}
          {hasOpenAiOAuth ? "Reconnect ChatGPT" : "Connect ChatGPT"}
        </Button>

        {hasOpenAiOAuth && (
          <span className="inline-flex items-center gap-1 text-xs font-medium text-emerald-600">
            <CheckCircle2 className="h-3.5 w-3.5" />
            Ready to save the provider
          </span>
        )}
      </div>

      {flow && (
        <div className="mt-4 space-y-3 rounded-md border bg-primary/[0.03] p-3">
          {flowStatus?.message && (
            <p className="text-xs text-muted-foreground">{flowStatus.message}</p>
          )}
          <div className="space-y-1 text-xs text-muted-foreground">
            <p>1. Finish the login in the opened browser tab or on another device.</p>
            <p>
              2. If the browser is on this same machine, it should land on an
              {" "}authentication-success page at <code className="rounded bg-muted px-1 py-0.5 font-mono">localhost:1455</code>.
            </p>
            <p>3. If automatic completion does not happen after a few seconds, paste the final callback URL or query string below.</p>
          </div>

          <div className="flex gap-2">
            <Input
              readOnly
              value={flow.authorize_url}
              className="font-mono text-xs"
            />
            <Button type="button" variant="outline" size="sm" onClick={copyLoginUrl}>
              <Copy className="h-3.5 w-3.5" />
            </Button>
          </div>

          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
              Callback URL or Query
            </label>
            <Textarea
              value={callbackInput}
              onChange={(event) => setCallbackInput(event.target.value)}
              placeholder="http://localhost:1455/auth/callback?code=...&state=..."
              className="min-h-24 font-mono text-xs"
            />
          </div>

          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              size="sm"
              onClick={handleComplete}
              disabled={completeOpenAiOAuth.isPending || !callbackInput.trim()}
            >
              {completeOpenAiOAuth.isPending ? (
                <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" />
              ) : (
                "Complete Login"
              )}
            </Button>
            <Button type="button" size="sm" variant="outline" asChild>
              <a href={flow.authorize_url} target="_blank" rel="noreferrer">
                Reopen Login Page
              </a>
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
