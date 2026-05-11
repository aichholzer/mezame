import { useEffect, useState } from 'react';
import { InputRow } from '@/features/InputRow';
import { LogPane } from '@/features/LogPane';
import { NewSessionDialog } from '@/features/NewSessionDialog';
import { TabBar } from '@/features/TabBar';
import { useAttentionBadge } from '@/hooks/useAttentionBadge';
import { useKeyboardInset } from '@/hooks/useKeyboardInset';
import { mezameActions, useMezame } from '@/hooks/useMezame';

export const App = () => {
  const { sessions, closed, activeId, activeSession } = useMezame();
  const [newSessionOpen, setNewSessionOpen] = useState(false);

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
      className="flex h-full h-[100dvh] min-h-0 flex-col"
      style={{
        // Top/left/right safe-area padding so the header and side
        // gutters clear the notch and rounded-screen insets on
        // iOS. Bottom inset is handled inside the floating composer,
        // not here: the scrollable log wants the full height and
        // applies its own padding around the composer footprint.
        paddingTop: 'var(--mz-safe-top)',
        paddingLeft: 'var(--mz-safe-left)',
        paddingRight: 'var(--mz-safe-right)'
      }}
    >
      {/* Single centred content column. Both the header (tab bar) and
       * the chat pane live inside it, so they share the same width
       * cap and can fill it freely without each needing their own
       * max-width. */}
      <div className="mx-auto flex w-full min-h-0 flex-1 flex-col max-w-[1600px]">
        <TabBar
          sessions={sessions}
          activeId={activeId}
          closed={closed}
          onActivate={mezameActions.activate}
          onClose={mezameActions.closeSession}
          onRename={mezameActions.renameSession}
          onRestore={mezameActions.restoreFromHistory}
          onForget={mezameActions.forgetHistory}
          onNewTab={() => setNewSessionOpen(true)}
        />

        {/* Relative so the floating composer inside can anchor to the
         * chat area rather than the whole viewport. */}
        <main className="relative flex min-h-0 flex-1 flex-col">
          {sessions.map((s) => (
            <LogPane key={s.id} session={s} isActive={s.id === activeId} />
          ))}

          <InputRow session={activeSession} onSubmit={mezameActions.sendPrompt} />
        </main>
      </div>

      <NewSessionDialog
        open={newSessionOpen}
        onOpenChange={setNewSessionOpen}
        onCreate={(cwd, name) => mezameActions.newSession(cwd, name)}
      />
    </div>
  );
};
