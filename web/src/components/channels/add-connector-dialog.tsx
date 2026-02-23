"use client";

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
};

interface AddConnectorDialogProps {
  kind: string;
  label: string;
}

export function AddConnectorDialog({ kind, label }: AddConnectorDialogProps) {
  const [open, setOpen] = useState(false);
  const addConnector = useAddConnector();
  const form = useForm<AddConnectorFormValues>({
    defaultValues: {
      connectorId: "",
      token: "",
    },
  });

  const onSubmit = async (values: AddConnectorFormValues) => {
    try {
      await addConnector.mutateAsync({
        kind,
        connectorId: values.connectorId,
        token: values.token,
      });
      toast.success(`${label} connector added`);
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
            <FormField
              control={form.control}
              name="token"
              rules={{ required: "Token is required" }}
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
