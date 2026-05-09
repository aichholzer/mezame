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

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreate: (cwd: string | null, name: string | null) => void;
};

export const NewSessionDialog = ({ open, onOpenChange, onCreate }: Props) => {
  const [name, setName] = useState('');
  const [cwd, setCwd] = useState('');
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) {
      setName('');
      setCwd('');
      // Radix auto-focuses the first focusable element; that's the close
      // button. Move focus to the name input on the next tick.
      setTimeout(() => nameRef.current?.focus(), 0);
    }
  }, [open]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    onCreate(cwd.trim() || null, name.trim() || null);
    onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
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
            />
          </div>
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
