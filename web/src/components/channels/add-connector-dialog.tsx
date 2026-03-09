import { useState } from "react";
import { useForm } from "react-hook-form";
import { Plus } from "lucide-react";
import { toast } from "sonner";

import { useAddConnector } from "@/hooks/use-api";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form";
import { Input } from "@/components/ui/input";

type AddConnectorFormValues = {
  connectorId: string;
  token: string;
  appId: string;
  appSecret: string;
  clientId: string;
  clientSecret: string;
  botId: string;
  secret: string;
};

interface AddConnectorDialogProps {
  kind: string;
  label: string;
  onAdded?: () => void;
}

export function AddConnectorDialog({ kind, label, onAdded }: AddConnectorDialogProps) {
  const [open, setOpen] = useState(false);
  const addConnector = useAddConnector();
  const form = useForm<AddConnectorFormValues>({
    defaultValues: {
      connectorId: "",
      token: "",
      appId: "",
      appSecret: "",
      clientId: "",
      clientSecret: "",
      botId: "",
      secret: "",
    },
  });

  const isChineseChannel = ["feishu", "dingtalk", "wecom"].includes(kind);

  const onSubmit = async (values: AddConnectorFormValues) => {
    try {
      await addConnector.mutateAsync({
        kind,
        connectorId: values.connectorId,
        ...(values.token ? { token: values.token } : {}),
        ...(kind === "feishu" ? { appId: values.appId, appSecret: values.appSecret } : {}),
        ...(kind === "dingtalk" ? { clientId: values.clientId, clientSecret: values.clientSecret } : {}),
        ...(kind === "wecom" ? { botId: values.botId, secret: values.secret } : {}),
      });
      toast.success(`${label} connector added`);
      onAdded?.();
      form.reset();
      setOpen(false);
    } catch {
      toast.error(`Failed to add ${label} connector`);
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="h-8">
          <Plus className="h-4 w-4" />
          Add Bot
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Add {label} Connector</DialogTitle>
          <DialogDescription>Create a new bot connector for this channel.</DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="grid gap-4">
            <FormField
              control={form.control}
              name="connectorId"
              rules={{ required: "Connector ID is required" }}
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Connector ID</FormLabel>
                  <FormControl>
                    <Input placeholder="tg_support" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            {kind === "feishu" ? (
              <>
                <FormField
                  control={form.control}
                  name="appId"
                  rules={{ required: "App ID is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>App ID</FormLabel>
                      <FormControl>
                        <Input placeholder="cli_xxx" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
                <FormField
                  control={form.control}
                  name="appSecret"
                  rules={{ required: "App Secret is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>App Secret</FormLabel>
                      <FormControl>
                        <Input type="password" placeholder="App secret from Feishu" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
              </>
            ) : kind === "dingtalk" ? (
              <>
                <FormField
                  control={form.control}
                  name="clientId"
                  rules={{ required: "Client ID is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Client ID</FormLabel>
                      <FormControl>
                        <Input placeholder="AppKey from DingTalk" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
                <FormField
                  control={form.control}
                  name="clientSecret"
                  rules={{ required: "Client Secret is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Client Secret</FormLabel>
                      <FormControl>
                        <Input type="password" placeholder="AppSecret from DingTalk" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
              </>
            ) : kind === "wecom" ? (
              <>
                <FormField
                  control={form.control}
                  name="botId"
                  rules={{ required: "Bot ID is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Bot ID</FormLabel>
                      <FormControl>
                        <Input placeholder="Bot ID from WeCom Admin" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
                <FormField
                  control={form.control}
                  name="secret"
                  rules={{ required: "Secret is required" }}
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Secret</FormLabel>
                      <FormControl>
                        <Input type="password" placeholder="Bot secret" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
              </>
            ) : (
              <FormField
                control={form.control}
                name="token"
                rules={{ required: isChineseChannel ? false : "Token is required" }}
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Bot Token</FormLabel>
                    <FormControl>
                      <Input type="password" placeholder="123456:ABC..." {...field} />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
            )}
            <DialogFooter>
              <Button type="submit" disabled={addConnector.isPending}>
                Add Connector
              </Button>
            </DialogFooter>
          </form>
        </Form>
      </DialogContent>
    </Dialog>
  );
}
