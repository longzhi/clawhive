"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Brain, Loader2, CheckCircle, Key, ShieldCheck } from "lucide-react";
import { useAuthStatus, useProviders, useTestProvider, useSetProviderKey } from "@/hooks/use-api";
import { toast } from "sonner";
import { useState } from "react";

export default function ProvidersPage() {
  const { data: providers, isLoading } = useProviders();
  const { data: authStatus } = useAuthStatus();
  const testProvider = useTestProvider();
  const setProviderKey = useSetProviderKey();
  const [keys, setKeys] = useState<Record<string, string>>({});

  const handleSaveKey = async (id: string) => {
    const apiKey = keys[id];
    if (!apiKey) return;
    
    try {
      await setProviderKey.mutateAsync({ id, apiKey });
      toast.success("API key saved");
      setKeys(prev => ({ ...prev, [id]: "" }));
    } catch (e) {
      toast.error("Failed to save API key");
    }
  };

  const handleTest = async (id: string) => {
    try {
      const result = await testProvider.mutateAsync(id);
      if (result.ok) {
        toast.success(`Provider ${id} is working correctly`);
      } else {
        toast.error(`Provider ${id} failed: ${result.message}`);
      }
    } catch (e) {
      toast.error(`Failed to test provider ${id}`);
    }
  };

  const authProfileForProvider = (providerId: string) =>
    authStatus?.profiles.find((p) => p.provider === providerId && p.active);

  const loginHint = (providerId: string) =>
    providerId === "openai" ? "clawhive auth login openai" : "clawhive auth login anthropic";

  const handleShowLoginHint = (providerId: string) => {
    toast.message(`Use CLI: ${loginHint(providerId)}`);
  };

  if (isLoading) {
    return (
      <div className="flex justify-center p-8">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    );
  }

  return (
    <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
      {providers?.map((provider) => (
        <Card key={provider.provider_id}>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <div className="flex flex-col space-y-1">
              <CardTitle className="capitalize">{provider.provider_id}</CardTitle>
              <CardDescription className="font-mono text-xs truncate max-w-[200px]">
                {provider.api_base}
              </CardDescription>
            </div>
            <Brain className="h-6 w-6 text-muted-foreground" />
          </CardHeader>
          <CardContent className="grid gap-4 pt-4">
            <div className="flex items-center justify-between">
              <span className="text-sm text-muted-foreground">API Key</span>
              <Badge 
                variant={provider.key_configured ? "default" : "secondary"} 
                className={provider.key_configured ? "bg-green-500 hover:bg-green-600" : "bg-amber-500 hover:bg-amber-600 text-white"}
              >
                {provider.key_configured ? "Configured" : "Not Set"}
              </Badge>
            </div>

            <div className="flex items-center justify-between">
              <span className="text-sm text-muted-foreground">OAuth / Session</span>
              {authProfileForProvider(provider.provider_id) ? (
                <Badge className="bg-emerald-600 hover:bg-emerald-700">
                  <ShieldCheck className="mr-1 h-3.5 w-3.5" />
                  Connected
                </Badge>
              ) : (
                <Button
                  variant="secondary"
                  size="sm"
                  className="h-7"
                  onClick={() => handleShowLoginHint(provider.provider_id)}
                >
                  Login
                </Button>
              )}
            </div>

            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <div className="relative flex-1">
                  <Key className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                  <Input
                    type="password"
                    placeholder="Enter API key..."
                    className="pl-9 h-9 text-sm"
                    value={keys[provider.provider_id] || ""}
                    onChange={(e) => setKeys(prev => ({ ...prev, [provider.provider_id]: e.target.value }))}
                  />
                </div>
                <Button 
                  size="sm" 
                  className="h-9"
                  onClick={() => handleSaveKey(provider.provider_id)}
                  disabled={setProviderKey.isPending || !keys[provider.provider_id]}
                >
                  Save
                </Button>
              </div>
              {provider.api_key_env && (
                <span className="text-xs text-muted-foreground">Sets {provider.api_key_env}</span>
              )}
            </div>
            
            <div className="flex flex-col gap-2">
              <span className="text-sm text-muted-foreground">Models</span>
              <div className="flex flex-wrap gap-1">
                {provider.models.map((model) => (
                  <Badge key={model} variant="outline" className="text-[10px] px-1">
                    {model}
                  </Badge>
                ))}
              </div>
            </div>

            <Button 
              variant="outline" 
              size="sm" 
              className="w-full mt-2"
              onClick={() => handleTest(provider.provider_id)}
              disabled={testProvider.isPending}
            >
              {testProvider.isPending ? (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              ) : (
                <CheckCircle className="mr-2 h-4 w-4" />
              )}
              Test Connection
            </Button>
          </CardContent>
        </Card>
      ))}
      
      {providers?.length === 0 && (
        <div className="col-span-full text-center text-muted-foreground p-8">
          No providers configured
        </div>
      )}
    </div>
  );
}
