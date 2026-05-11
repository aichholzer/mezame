import { MenuIcon } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { InputRow } from '@/features/InputRow';
import { LogPane } from '@/features/LogPane';
import { NewSessionDialog } from '@/features/NewSessionDialog';
import { SideBar } from '@/features/SideBar';
import { useAttentionBadge } from '@/hooks/useAttentionBadge';
import { useKeyboardInset } from '@/hooks/useKeyboardInset';
import { mezameActions, useMezame } from '@/hooks/useMezame';

export const App = () => {
  const { sessions, closed, activeId, activeSession } = useMezame();
  const [newSessionOpen, setNewSessionOpen] = useState(false);
  // Mobile-only: drawer state for the sidebar. Desktop ignores it
  // (the sidebar is always rendered and the transform-based hiding
  // is overridden at `md:`).
  const [sidebarOpen, setSidebarOpen] = useState(false);

  useAttentionBadge();
  useKeyboardInset();

  // Mirror the browser tab's visibility onto
  // `<html data-visibility="visible|hidden">` so CSS can pause
  // animations when the user has switched away. See
  // `.tab-busy-border` in index.css.
  useEffect(() => {
    const onVisibility = () => {
      document.documentElement.dataset.visibility = document.visibilityState;
    };
    document.addEventListener('visibilitychange', onVisibility);
    onVisibility();
    return () => document.removeEventListener('visibilitychange', onVisibility);
  }, []);

  useEffect(() => {
    void mezameActions.init();
  }, []);

  return (
    <div
      className="flex h-full h-[100dvh] min-h-0"
      style={{
        // Top/left/right safe-area padding on the shell is handled
        // per-region now: the sidebar owns its own top/bottom/left
        // safe area, and the main column owns top/right. Bottom inset
        // for the composer is handled inside the floating composer
        // itself.
        paddingRight: 'var(--mz-safe-right)'
      }}
    >
      <SideBar
        sessions={sessions}
        activeId={activeId}
        closed={closed}
        onActivate={mezameActions.activate}
        onClose={mezameActions.closeSession}
        onRename={mezameActions.renameSession}
        onRestore={mezameActions.restoreFromHistory}
        onForget={mezameActions.forgetHistory}
        onNewTab={() => setNewSessionOpen(true)}
        isOpen={sidebarOpen}
        onRequestClose={() => setSidebarOpen(false)}
      />

      {/* Main column: the chat pane. Centred and width-capped so long
       * lines stay readable on ultra-wide monitors. Relative so the
       * floating composer inside can anchor to it. */}
      <main
        className="relative flex min-h-0 flex-1 flex-col"
        style={{ paddingTop: 'var(--mz-safe-top)' }}
      >
        {/* Mobile burger: pinned top-left of the main column, hidden on
         * desktop where the sidebar is always visible. The sidebar has
         * its own safe-area padding, so this button sits above the
         * notch by matching it. */}
        <div
          className="absolute left-3 top-3 z-20 md:hidden"
          style={{ top: 'calc(0.75rem + var(--mz-safe-top))' }}
        >
          <Button
            size="icon"
            variant="outline"
            className="size-10 text-[color:var(--primary)]"
            onClick={() => setSidebarOpen(true)}
            aria-label="Open sidebar"
          >
            <MenuIcon className="size-5" />
          </Button>
        </div>

        <div className="mx-auto flex w-full min-h-0 flex-1 flex-col">
          {sessions.map((s) => (
            <LogPane key={s.id} session={s} isActive={s.id === activeId} />
          ))}

          <InputRow session={activeSession} onSubmit={mezameActions.sendPrompt} />
        </div>
      </main>

      <NewSessionDialog
        open={newSessionOpen}
        onOpenChange={setNewSessionOpen}
        onCreate={(cwd, name) => mezameActions.newSession(cwd, name)}
      />
    </div>
  );
};
