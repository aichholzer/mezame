import { MenuIcon } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { InputRow } from '@/features/InputRow';
import { LogPane } from '@/features/LogPane';
import { NewSessionDialog } from '@/features/NewSessionDialog';
import { NotificationsPrompt } from '@/features/NotificationsPrompt';
import { SideBar } from '@/features/SideBar';
import { useAttentionBadge } from '@/hooks/useAttentionBadge';
import { useKeyboardInset } from '@/hooks/useKeyboardInset';
import { useNotifications } from '@/hooks/useNotifications';
import { useApplyTheme } from '@/hooks/useTheme';
import { LoginGate } from '@/features/LoginGate';
import { mezameActions, useMezame } from '@/hooks/useMezame';
import { checkMe, useAuth } from '@/lib/auth';
import { initSettings } from '@/lib/settings';

export const App = () => {
  const auth = useAuth();
  const { sessions, closed, activeId, activeSession } = useMezame();
  const [newSessionOpen, setNewSessionOpen] = useState(false);
  // Mobile-only: drawer state for the sidebar. Desktop ignores it
  // (the sidebar is always rendered and the transform-based hiding
  // is overridden at `md:`).
  const [sidebarOpen, setSidebarOpen] = useState(false);

  useAttentionBadge();
  useKeyboardInset();
  useNotifications();
  useApplyTheme();

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

  // Who holds the cookie, once per load; the answer moves the state off
  // `unknown` and decides which shell renders.
  useEffect(() => {
    void checkMe();
  }, []);

  // Each entry into the signed-in state seeds both stores: the first
  // load, and every later login after a logout or an expiry emptied
  // them.
  useEffect(() => {
    if (auth.status === 'user') {
      void mezameActions.init();
      void initSettings();
    }
  }, [auth.status]);

  if (auth.status !== 'user') {
    // A blank shell while `/me` is in flight, so the page never flashes
    // the login form at someone who is signed in; the form once it is
    // known nobody is.
    return auth.status === 'anonymous' ? <LoginGate /> : <div className="h-[100dvh]" />;
  }

  return (
    <div
      className="flex h-full h-[100dvh] min-h-0 w-full max-w-full overflow-x-clip"
      style={{
        // Top/left/right safe-area padding on the shell is handled
        // per-region: the sidebar owns its own top/bottom/left safe
        // area, and the main column owns top/right.
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
       * floating composer inside can anchor to it. The desktop left
       * margin reserves space for the floating sidebar (it lives on
       * `position: fixed` so the chat column would otherwise slide
       * under it). The custom property is updated live by
       * `useSidebarWidth` while the user drags. Mobile keeps the
       * full width since the sidebar is a drawer there. */}
      <main
        className="relative flex min-h-0 min-w-0 flex-1 flex-col pl-2 pr-2 pb-2 md:pl-0 md:pr-5 md:pb-5"
        style={{
          paddingTop: 'calc(20px + var(--mz-safe-top))',
          marginLeft: 'var(--mz-main-left, 0)'
        }}
      >
        {/* Mobile burger: pinned top-left of the main column, hidden on
         * desktop where the sidebar is always visible. */}
        <div
          className="absolute left-3 top-3 z-20 md:hidden"
          style={{ top: 'calc(0.75rem + var(--mz-safe-top))' }}
        >
          <Button
            size="icon"
            variant="outline"
            className="size-10 rounded-full text-[color:var(--primary)]"
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
        onCreate={(name) => mezameActions.newSession(name)}
      />
      <NotificationsPrompt />
    </div>
  );
};
