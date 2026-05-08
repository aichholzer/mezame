import type { ComponentProps } from 'react';
import { cn } from '@/lib/utils';

const Input = ({ className, type, ...props }: ComponentProps<'input'>) => (
  <input
    type={type}
    data-slot="input"
    className={cn(
      'flex h-9 w-full rounded-md border bg-input px-3 py-1 text-sm shadow-xs transition-colors',
      'placeholder:text-muted-foreground',
      'focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring',
      'disabled:cursor-not-allowed disabled:opacity-50',
      'file:border-0 file:bg-transparent file:text-sm file:font-medium file:text-foreground',
      className
    )}
    {...props}
  />
);

export { Input };
