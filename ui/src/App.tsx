import { useEffect, useState } from 'react';
import { InputRow } from '@/features/InputRow';
import { LogPane } from '@/features/LogPane';
import { NewSessionDialog } from '@/features/NewSessionDialog';
import { TabBar } from '@/features/TabBar';
import { useAttentionBadge } from '@/hooks/useAttentionBadge';
import { okiroActions, useOkiro } from '@/hooks/useOkiro';

export const App = () => {
  const { sessions, closed, activeId, activeSession } = useOkiro();
  const [newSessionOpen, setNewSessionOpen] = useState(false);

  useAttentionBadge();

  useEffect(() => {
    void okiroActions.init();
  }, []);

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Single centred content column. Both the header (tab bar) and
       * the chat pane live inside it, so they share the same width
       * cap and can fill it freely without each needing their own
       * max-width. */}
      <div className="mx-auto flex w-full min-h-0 flex-1 flex-col max-w-[1600px]">
        <TabBar
          sessions={sessions}
          activeId={activeId}
          closed={closed}
          onActivate={okiroActions.activate}
          onClose={okiroActions.closeSession}
          onRename={okiroActions.renameSession}
          onRestore={okiroActions.restoreFromHistory}
          onForget={okiroActions.forgetHistory}
          onNewTab={() => setNewSessionOpen(true)}
        />

        {/* Relative so the floating composer inside can anchor to the
         * chat area rather than the whole viewport. */}
        <main className="relative flex min-h-0 flex-1 flex-col">
          {sessions.map((s) => (
            <LogPane key={s.id} session={s} isActive={s.id === activeId} />
          ))}

          <InputRow session={activeSession} onSubmit={okiroActions.sendPrompt} />
        </main>
      </div>

      <NewSessionDialog
        open={newSessionOpen}
        onOpenChange={setNewSessionOpen}
        onCreate={(cwd, name) => okiroActions.newSession(cwd, name)}
      />
    </div>
  );
};
