import { Link, useLocation } from "react-router-dom";
import { ChevronRight, Home } from "lucide-react";

const ROUTE_LABELS: Record<string, string> = {
  "/": "Dashboard",
  "/agents": "Agents",
  "/sessions": "Sessions",
  "/schedules": "Schedules",
  "/channels": "Channels",
  "/providers": "Providers",
  "/routing": "Routing",
  "/settings": "Settings",
  "/skills": "Skills",
  "/login": "Login",
};

export function Breadcrumbs() {
  const { pathname } = useLocation();

  // Don't show breadcrumbs on dashboard (it's home)
  if (pathname === "/") return null;

  const label = ROUTE_LABELS[pathname] || pathname.slice(1);

  return (
    <nav className="flex items-center gap-1.5 text-sm text-muted-foreground">
      <Link
        to="/"
        className="flex items-center gap-1 hover:text-foreground transition-colors"
      >
        <Home className="h-3.5 w-3.5" />
        <span className="hidden sm:inline">Dashboard</span>
      </Link>
      <ChevronRight className="h-3.5 w-3.5" />
      <span className="text-foreground font-medium">{label}</span>
    </nav>
  );
}
