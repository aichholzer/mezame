import type { ComponentProps } from 'react';
import { cn } from '@/lib/utils';

const Textarea = ({ className, ...props }: ComponentProps<'textarea'>) => (
  <textarea
    data-slot="textarea"
    className={cn(
      'flex min-h-[2.5rem] w-full resize-none rounded-md border bg-input px-3 py-2 text-sm shadow-xs transition-colors',
      'placeholder:text-muted-foreground',
      'focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring',
      'disabled:cursor-not-allowed disabled:opacity-50',
      className
    )}
    {...props}
  />
);

export { Textarea };
