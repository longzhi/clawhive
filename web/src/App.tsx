import { Routes, Route } from "react-router-dom";
import { Providers } from "@/lib/providers";
import { Sidebar } from "@/components/layout/sidebar";
import { Topbar } from "@/components/layout/topbar";
import { MobileNav } from "@/components/layout/mobile-nav";
import { Toaster } from "@/components/ui/sonner";

import Dashboard from "@/pages/Dashboard";
import Agents from "@/pages/Agents";
import Providers_ from "@/pages/Providers";
import Channels from "@/pages/Channels";
import Routing from "@/pages/Routing";
import Sessions from "@/pages/Sessions";
import Schedules from "@/pages/Schedules";
import Setup from "@/pages/Setup";

export default function App() {
  return (
    <Providers>
      <div className="flex min-h-screen w-full flex-col bg-muted/40">
        <Sidebar />
        <div className="flex flex-col sm:gap-4 sm:py-4 sm:pl-[60px] lg:pl-[220px]">
          <Topbar />
          <main className="grid flex-1 items-start gap-4 p-4 sm:px-6 sm:py-0 md:gap-8 pb-20 md:pb-4">
            <Routes>
              <Route path="/" element={<Dashboard />} />
              <Route path="/agents" element={<Agents />} />
              <Route path="/providers" element={<Providers_ />} />
              <Route path="/channels" element={<Channels />} />
              <Route path="/routing" element={<Routing />} />
              <Route path="/sessions" element={<Sessions />} />
              <Route path="/schedules" element={<Schedules />} />
              <Route path="/setup" element={<Setup />} />
            </Routes>
          </main>
        </div>
        <MobileNav />
      </div>
      <Toaster />
    </Providers>
  );
}
