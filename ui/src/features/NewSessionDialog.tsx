import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { fetchAgents } from '@/lib/agents';

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreate: (cwd: string | null, name: string | null, agent: string | null) => void;
};

export const NewSessionDialog = ({ open, onOpenChange, onCreate }: Props) => {
  const [name, setName] = useState('');
  const [cwd, setCwd] = useState('');
  // Configured agents and the current selection. `agent` is '' until the
  // list loads (and stays '' when only one agent is configured, meaning
  // "let the server pick its default"). When the user has more than one
  // agent, the picker is shown and seeded with the server's default.
  const [agents, setAgents] = useState<string[]>([]);
  const [agent, setAgent] = useState('');
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!open) {
      return;
    }
    setName('');
    setCwd('');
    setAgent('');
    setAgents([]);
    // Radix auto-focuses the first focusable element; that's the close
    // button. Move focus to the name input on the next tick.
    setTimeout(() => nameRef.current?.focus(), 0);
    // Load the configured agents each time the dialog opens so a
    // config.json edit is reflected without a reload.
    let cancelled = false;
    void fetchAgents().then((info) => {
      if (cancelled) {
        return;
      }
      setAgents(info.agents);
      setAgent(info.default ?? info.agents[0] ?? '');
    });
    return () => {
      cancelled = true;
    };
  }, [open]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    // Only send an explicit agent when the user could actually choose;
    // with zero or one configured agent, null lets the server use its
    // default and keeps the WS URL clean.
    const chosenAgent = agents.length > 1 && agent ? agent : null;
    onCreate(cwd.trim() || null, name.trim() || null, chosenAgent);
    onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent variant="sheet">
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>
            A fresh agent subprocess is spawned per session.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={submit} className="flex flex-col gap-3">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="ns-name">Session name (optional)</Label>
            <Input
              id="ns-name"
              ref={nameRef}
              value={name}
              onChange={(e) => setName(e.target.value)}
              autoComplete="off"
              className="text-base md:text-sm"
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="ns-cwd">Working directory (optional)</Label>
            <Input
              id="ns-cwd"
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
              autoComplete="off"
              placeholder="leave blank for mezame's directory"
              className="text-base md:text-sm"
            />
          </div>
          {/* Agent picker only appears when the user has configured more
           * than one agent; with a single agent there is nothing to
           * choose and the row would be noise. */}
          {agents.length > 1 && (
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="ns-agent">Agent</Label>
              <select
                id="ns-agent"
                value={agent}
                onChange={(e) => setAgent(e.target.value)}
                className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-base shadow-sm transition-colors focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring md:text-sm"
              >
                {agents.map((a) => (
                  <option key={a} value={a}>
                    {a}
                  </option>
                ))}
              </select>
            </div>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" size="sm" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" size="sm">
              Create
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
};
