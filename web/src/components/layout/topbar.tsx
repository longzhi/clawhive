'use client';

import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTitle, SheetTrigger } from '@/components/ui/sheet';
import { VisuallyHidden } from '@radix-ui/react-visually-hidden';
import { Sidebar } from './sidebar';
import { usePathname } from 'next/navigation';

export function Topbar() {
  const pathname = usePathname();
  
  const getPageTitle = (path: string) => {
    if (path === '/') return 'Dashboard';
    if (path.startsWith('/agents')) return 'Agents';
    if (path.startsWith('/sessions')) return 'Sessions';
    if (path.startsWith('/schedules')) return 'Schedules';
    if (path.startsWith('/channels')) return 'Channels';
    if (path.startsWith('/providers')) return 'Providers';
    if (path.startsWith('/routing')) return 'Routing';
    if (path.startsWith('/settings')) return 'Settings';
    return 'Clawhive';
  };

  return (
    <header className="h-14 border-b border-border bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 flex items-center justify-between px-4 sticky top-0 z-20">
      <div className="flex items-center gap-4">
        <Sheet>
          <SheetTrigger asChild>
            <Button variant="ghost" size="icon" className="md:hidden">
              <Menu className="h-5 w-5" />
              <span className="sr-only">Toggle menu</span>
            </Button>
          </SheetTrigger>
          <SheetContent side="left" className="p-0 w-[280px] bg-sidebar border-r border-sidebar-border">
            <VisuallyHidden><SheetTitle>Navigation</SheetTitle></VisuallyHidden>
            <Sidebar mobile />
          </SheetContent>
        </Sheet>
        
        <h1 className="text-lg font-semibold md:text-xl">
          <span className="md:hidden">Clawhive</span>
          <span className="hidden md:inline-block">{getPageTitle(pathname)}</span>
        </h1>
      </div>

      <div className="flex items-center gap-2">
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span className="relative flex h-2.5 w-2.5">
            <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-green-400 opacity-75"></span>
            <span className="relative inline-flex rounded-full h-2.5 w-2.5 bg-green-500"></span>
          </span>
          <span className="hidden sm:inline">Connected</span>
        </div>
      </div>
    </header>
  );
}
