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
  onCreate: (name: string | null) => void;
};

export const NewSessionDialog = ({ open, onOpenChange, onCreate }: Props) => {
  const [name, setName] = useState('');
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) {
      setName('');
      // Radix auto-focuses the first focusable element; that's the close
      // button. Move focus to the name input on the next tick.
      setTimeout(() => nameRef.current?.focus(), 0);
    }
  }, [open]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    onCreate(name.trim() || null);
    onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent variant="sheet">
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>
            A session holds its own conversation. Open as many as you like.
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
