import * as DialogPrimitive from '@radix-ui/react-dialog';
import { XIcon } from 'lucide-react';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/utils';

const Dialog = ({ ...props }: ComponentProps<typeof DialogPrimitive.Root>) => (
  <DialogPrimitive.Root data-slot="dialog" {...props} />
);

const DialogTrigger = ({ ...props }: ComponentProps<typeof DialogPrimitive.Trigger>) => (
  <DialogPrimitive.Trigger data-slot="dialog-trigger" {...props} />
);

const DialogPortal = ({ ...props }: ComponentProps<typeof DialogPrimitive.Portal>) => (
  <DialogPrimitive.Portal data-slot="dialog-portal" {...props} />
);

const DialogClose = ({ ...props }: ComponentProps<typeof DialogPrimitive.Close>) => (
  <DialogPrimitive.Close data-slot="dialog-close" {...props} />
);

const DialogOverlay = ({ className, ...props }: ComponentProps<typeof DialogPrimitive.Overlay>) => (
  <DialogPrimitive.Overlay
    data-slot="dialog-overlay"
    className={cn(
      'fixed inset-0 z-50 bg-black/55',
      'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0',
      className
    )}
    {...props}
  />
);

const DialogContent = ({
  className,
  variant = 'modal',
  children,
  ...props
}: ComponentProps<typeof DialogPrimitive.Content> & { variant?: 'modal' | 'sheet' }) => (
  <DialogPortal>
    <DialogOverlay />
    <DialogPrimitive.Content
      data-slot="dialog-content"
      className={cn(
        // Desktop: centred modal. On mobile the `sheet` variant slides
        // up from the bottom and fills the viewport width; the `modal`
        // variant keeps the centred behaviour everywhere.
        'fixed z-50 grid gap-3 border bg-card p-4 shadow-lg duration-200',
        variant === 'sheet'
          ? cn(
            // Mobile: full-width bottom sheet. Round only the top
            // corners so the card visually merges with the bottom
            // edge. Safe-area bottom padding keeps content above
            // the iOS home indicator.
            'bottom-0 left-0 right-0 w-full max-w-full rounded-t-md rounded-b-none',
            // Desktop: revert to centred modal.
            'md:top-1/2 md:left-1/2 md:bottom-auto md:right-auto md:-translate-x-1/2 md:-translate-y-1/2',
            'md:w-auto md:max-w-[min(90vw,380px)] md:rounded-md'
          )
          : cn(
            'top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 rounded-md',
            'w-full max-w-[min(90vw,380px)]'
          ),
        'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0 data-[state=open]:zoom-in-95 data-[state=closed]:zoom-out-95',
        className
      )}
      style={
        variant === 'sheet'
          ? { paddingBottom: 'calc(1rem + var(--mz-safe-bottom))' }
          : undefined
      }
      {...props}
    >
      {children}
      <DialogPrimitive.Close
        className={cn(
          'absolute top-3 right-3 rounded-sm opacity-60 transition-opacity',
          'hover:opacity-100 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-ring'
        )}
      >
        <XIcon className="size-4" />
        <span className="sr-only">Close</span>
      </DialogPrimitive.Close>
    </DialogPrimitive.Content>
  </DialogPortal>
);

const DialogHeader = ({ className, ...props }: ComponentProps<'div'>) => (
  <div data-slot="dialog-header" className={cn('flex flex-col gap-1 text-left', className)} {...props} />
);

const DialogFooter = ({ className, ...props }: ComponentProps<'div'>) => (
  <div
    data-slot="dialog-footer"
    className={cn('flex flex-row justify-end gap-2 pt-2', className)}
    {...props}
  />
);

const DialogTitle = ({ className, ...props }: ComponentProps<typeof DialogPrimitive.Title>) => (
  <DialogPrimitive.Title
    data-slot="dialog-title"
    className={cn('text-sm leading-none font-normal text-foreground', className)}
    {...props}
  />
);

const DialogDescription = ({ className, ...props }: ComponentProps<typeof DialogPrimitive.Description>) => (
  <DialogPrimitive.Description
    data-slot="dialog-description"
    className={cn('text-xs text-muted-foreground', className)}
    {...props}
  />
);

export {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogOverlay,
  DialogPortal,
  DialogTitle,
  DialogTrigger
};
