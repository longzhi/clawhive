import { Link, useLocation } from 'react-router-dom';
import { LayoutDashboard, Bot, MessageSquare, Settings, Sliders } from 'lucide-react';
import { cn } from '@/lib/utils';

const mobileTabs = [
  { href: '/', label: 'Dashboard', icon: LayoutDashboard },
  { href: '/agents', label: 'Agents', icon: Bot },
  { href: '/sessions', label: 'Sessions', icon: MessageSquare },
  { href: '/channels', label: 'Config', icon: Sliders },
  { href: '/settings', label: 'Settings', icon: Settings },
];

export function MobileNav() {
  const { pathname } = useLocation();

  return (
    <div className="md:hidden fixed bottom-0 left-0 right-0 h-16 bg-background border-t border-border flex items-center justify-around px-2 z-40">
      {mobileTabs.map((tab) => {
        const isActive = pathname === tab.href || (tab.label === 'Config' && ['/channels', '/providers', '/routing', '/schedules'].includes(pathname));
        return (
          <Link
            key={tab.href}
            to={tab.href}
            className={cn(
              "flex flex-col items-center justify-center w-full h-full gap-1 text-xs font-medium transition-colors",
              isActive ? "text-primary" : "text-muted-foreground hover:text-foreground"
            )}
          >
            <tab.icon className="h-5 w-5" />
            <span>{tab.label}</span>
          </Link>
        );
      })}
    </div>
  );
}
