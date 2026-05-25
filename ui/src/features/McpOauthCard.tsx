import { ExternalLinkIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import type { LogEntry, Session } from '@/types';
import { mezameActions } from '@/hooks/useMezame';

type Props = {
  session: Session;
  entry: Extract<LogEntry, { kind: 'mcp_oauth' }>;
};

/** Inline card surfacing an MCP server's OAuth request. The agent waits
 * out-of-band for the user to complete the auth flow in their browser;
 * we just bridge the gap. The Open button must be triggered by a user
 * gesture (browsers block popups otherwise), so we never auto-open. */
export const McpOauthCard = ({ session, entry }: Props) => {
  const open = () => {
    window.open(entry.url, '_blank', 'noopener,noreferrer');
    mezameActions.markOauthOpened(session.id, entry.id);
  };

  return (
    <div
      className={
        'my-3 rounded-sm border border-l-[3px] border-l-[color:var(--attn-permission)] ' +
        'bg-card px-3 py-2'
      }
    >
      <div className="mb-2 text-xs text-[color:var(--attn-permission)]">
        authorisation requested: {entry.serverName}
      </div>
      <div className="mb-2 break-all text-xs text-muted-foreground">{entry.url}</div>
      <div className="flex items-center gap-2">
        <Button size="sm" variant="outline" onClick={open} className="gap-1.5">
          <ExternalLinkIcon className="size-3.5" />
          {entry.opened ? 'Open again' : 'Open'}
        </Button>
        {entry.opened ? (
          <span className="text-[11px] text-muted-foreground">
            opened, complete the flow in your browser
          </span>
        ) : null}
      </div>
    </div>
  );
};
