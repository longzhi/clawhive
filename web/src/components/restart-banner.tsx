"use client";

import { AlertTriangle } from "lucide-react";

interface RestartBannerProps {
  visible: boolean;
}

export function RestartBanner({ visible }: RestartBannerProps) {
  if (!visible) {
    return null;
  }

  return (
    <div className="mb-4 flex items-center gap-2 rounded-md border border-amber-300 bg-amber-50 px-4 py-3 text-amber-900">
      <AlertTriangle className="h-4 w-4" />
      <p className="text-sm font-medium">Configuration changed. Restart required to apply connector updates.</p>
    </div>
  );
}
