import { cn } from '@/lib/utils';

// Project's bot avatar/icon. Renders the favicon image at whatever
// size the parent grants via Tailwind size utilities (`size-6`,
// `size-8`, etc.). Drop-in replacement for `lucide-react`'s `BotIcon`
// in places where we want the Mezame mark instead of a generic
// glyph.
//
// `aria-hidden` by default since the icon is decorative; pass
// `aria-label` if it ever needs to convey meaning on its own.

type Props = {
  className?: string;
};

export const BotIcon = ({ className }: Props) => (
  <img
    src="/favicon.png"
    alt=""
    aria-hidden="true"
    className={cn('object-contain', className)}
  />
);
