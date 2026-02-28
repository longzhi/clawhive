import { Link, useLocation } from 'react-router-dom';
import { LayoutDashboard, Bot, MessageSquare, Radio, Brain, GitBranch, CalendarClock, Settings } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Separator } from '@/components/ui/separator';

const navItems = [
  { href: '/', label: 'Dashboard', icon: LayoutDashboard },
  { href: '/agents', label: 'Agents', icon: Bot },
  { href: '/sessions', label: 'Sessions', icon: MessageSquare },
  { href: '/schedules', label: 'Schedules', icon: CalendarClock },
  { href: '/channels', label: 'Channels', icon: Radio },
  { href: '/providers', label: 'Providers', icon: Brain },
  { href: '/routing', label: 'Routing', icon: GitBranch },
];

interface SidebarProps {
  className?: string;
  mobile?: boolean;
}

export function Sidebar({ className, mobile }: SidebarProps) {
  const { pathname } = useLocation();

  return (
    <aside className={cn(
      "flex flex-col h-full bg-sidebar border-r border-sidebar-border transition-all duration-300",
      !mobile && "hidden md:flex fixed left-0 top-0 z-30 w-[60px] lg:w-[220px]",
      mobile && "w-full",
      className
    )}>
      <div className={cn(
        "h-14 flex items-center border-b border-sidebar-border",
        mobile ? "px-6 justify-start" : "justify-center lg:justify-start lg:px-6"
      )}>
        <span className={cn("text-xl font-bold text-sidebar-foreground", !mobile && "hidden lg:block")}>🐝 Clawhive</span>
        <span className={cn("text-xl font-bold text-sidebar-foreground", !mobile && "lg:hidden", mobile && "hidden")}>🐝</span>
      </div>
      
      <nav className="flex-1 py-4 flex flex-col gap-1 px-2">
        {navItems.map((item) => {
          const isActive = pathname === item.href;
          return (
            <Link
              key={item.href}
              to={item.href}
              className={cn(
                "flex items-center gap-3 px-3 py-2 rounded-md text-sm font-medium transition-colors relative group",
                isActive 
                  ? "bg-sidebar-accent text-sidebar-primary" 
                  : "text-sidebar-foreground/70 hover:bg-sidebar-accent hover:text-sidebar-foreground"
              )}
            >
              {isActive && (
                <div className="absolute left-0 top-0 bottom-0 w-1 bg-sidebar-primary rounded-r-full" />
              )}
              <item.icon className={cn("h-5 w-5", isActive ? "text-sidebar-primary" : "text-sidebar-foreground/70")} />
              <span className={cn(!mobile && "hidden lg:block")}>{item.label}</span>
            </Link>
          );
        })}
        
        <div className="my-2 px-2">
          <Separator className="bg-sidebar-border" />
        </div>

        <Link
          to="/settings"
          className={cn(
            "flex items-center gap-3 px-3 py-2 rounded-md text-sm font-medium transition-colors relative",
            pathname === '/settings'
              ? "bg-sidebar-accent text-sidebar-primary"
              : "text-sidebar-foreground/70 hover:bg-sidebar-accent hover:text-sidebar-foreground"
          )}
        >
          {pathname === '/settings' && (
            <div className="absolute left-0 top-0 bottom-0 w-1 bg-sidebar-primary rounded-r-full" />
          )}
          <Settings className={cn("h-5 w-5", pathname === '/settings' ? "text-sidebar-primary" : "text-sidebar-foreground/70")} />
          <span className={cn(!mobile && "hidden lg:block")}>Settings</span>
        </Link>
      </nav>
    </aside>
  );
}
