import { PaperclipIcon, SendIcon, SettingsIcon, XIcon } from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuTrigger
} from '@/components/ui/dropdown-menu';
import { Textarea } from '@/components/ui/textarea';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { CwdChip } from '@/features/CwdChip';
import { ModeModelSelectors } from '@/features/ModeModelSelectors';
import { SlashAutocomplete, type SlashAutocompleteHandle } from '@/features/SlashAutocomplete';
import { useIsMobile } from '@/hooks/useIsMobile';
import {
  attachmentToBlock,
  cleanup,
  describeRejection,
  fileToAttachment,
  MAX_ATTACHMENTS,
  MAX_TOTAL_BYTES,
  type Attachment
} from '@/lib/attachments';
import { cn } from '@/lib/utils';
import type { PromptBlock, Session } from '@/types';

// Floating composer pinned to the bottom of the chat pane. The log
// keeps scrolling underneath it; a semi-transparent fill plus a
// backdrop blur lets the latest content show through faintly without
// ever hiding the composer itself.
//
// Desktop layout:
//   [ attachment chips (when any)                                 ]
//   [  textarea                                         [  send ] ]
//   [ [cwd chip] [attach]              [ Agent ] [ Model picker ] ]
//
// Mobile layout (below `md`):
//   [ attachment chips (when any)                                 ]
//   [  textarea                                                   ]
//   [ [cwd] [attach] [settings]                          [ send ] ]
//
// On mobile the top-right send button moves to the bottom row so it
// is within thumb reach, and the Agent/Model pickers move behind a
// settings icon that opens a DropdownMenu with both pickers stacked.
//
// Attachments come in three ways: paste an image from the clipboard,
// drop a file on the card, or click the paperclip to open a file
// picker. All three funnel through `stageFile` and render as chips
// above the textarea. The agent's advertised prompt capabilities
// (image, embeddedContext) gate which file types are accepted.

type Props = {
  session: Session | null;
  onSubmit: (text: string, blocks: PromptBlock[]) => void;
};

// Upper bound stays the same on all viewports; taller than this and the
// textarea scrolls internally rather than dominating the screen.
const MAX_ROWS = 8;

export const InputRow = ({ session, onSubmit }: Props) => {
  const isMobile = useIsMobile();
  // Start shorter on mobile so more of the log stays visible when the
  // keyboard is open. Desktop keeps the original 3-row resting height.
  const minRows = isMobile ? 2 : 3;

  const [value, setValue] = useState('');
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const slashRef = useRef<SlashAutocompleteHandle>(null);

  const caps = session?.promptCapabilities ?? {};
  const canAttachAnything = !!(caps.image || caps.embeddedContext);

  // The picker's `accept` attribute reflects the agent's advertised
  // capabilities so the OS file dialogue only offers files the agent
  // can actually take. `image/*` covers every image mime; the textish
  // branch in `fileToAttachment` recognises a small allowlist so we
  // hint at those mime types explicitly. When `embeddedContext` is on
  // we accept everything (binary embedded resources cover anything
  // not matched by the image / text branches).
  const acceptAttr = (() => {
    if (caps.embeddedContext) {
      // No restriction: any file is fine.
      return undefined;
    }
    if (caps.image) {
      return 'image/*';
    }
    return undefined;
  })();

  // Auto-dismiss any transient notice (rejection reason) after a few
  // seconds so the composer does not accumulate stale messages.
  useEffect(() => {
    if (!notice) {
      return;
    }
    const id = window.setTimeout(() => setNotice(null), 4000);
    return () => clearTimeout(id);
  }, [notice]);

  // Revoke preview URLs on unmount so we do not leak blobs.
  useEffect(
    () => () => {
      for (const att of attachments) {
        cleanup(att);
      }
    },
    // Intentionally empty: we only want cleanup on unmount. Per-item
    // removal is handled in `removeAttachment`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    []
  );

  useEffect(() => {
    if (session && !session.busy) {
      textareaRef.current?.focus();
    }
  }, [session?.id, session?.busy]);

  // Auto-grow between minRows and MAX_ROWS, scroll thereafter.
  useEffect(() => {
    const el = textareaRef.current;
    if (!el) {
      return;
    }
    const computed = getComputedStyle(el);
    const lineHeight = parseFloat(computed.lineHeight || '20');
    const paddingY = parseFloat(computed.paddingTop) + parseFloat(computed.paddingBottom);
    el.style.height = 'auto';
    const minPx = lineHeight * minRows + paddingY;
    const maxPx = lineHeight * MAX_ROWS + paddingY;
    el.style.height = `${Math.min(Math.max(el.scrollHeight, minPx), maxPx)}px`;
  }, [value, minRows]);

  const busy = !!session?.busy;
  const disabled = !session || busy;
  const canSend = !disabled && (value.trim().length > 0 || attachments.length > 0);

  /** Stage a single file: classify, quota-check, append. Returns false
   * when rejected so caller can stop at the first failure. */
  const stageFile = useCallback(
    (file: File, currentAtts: Attachment[]): Attachment[] | null => {
      if (currentAtts.length >= MAX_ATTACHMENTS) {
        setNotice(`Up to ${MAX_ATTACHMENTS} attachments per message.`);
        return null;
      }
      const totalAfter = currentAtts.reduce((acc, a) => acc + a.size, 0) + file.size;
      if (totalAfter > MAX_TOTAL_BYTES) {
        setNotice(`Total attachment size would exceed ${MAX_TOTAL_BYTES / 1024 / 1024} MB.`);
        return null;
      }
      const result = fileToAttachment(file, caps);
      if (!result.ok) {
        setNotice(describeRejection(result.reason));
        return null;
      }
      return [...currentAtts, result.attachment];
    },
    [caps]
  );

  const stageFiles = useCallback(
    (files: FileList | File[]) => {
      setAttachments((prev) => {
        let next = prev;
        for (const f of files) {
          const after = stageFile(f, next);
          if (!after) {
            return next;
          }
          next = after;
        }
        return next;
      });
    },
    [stageFile]
  );

  const removeAttachment = (id: string) => {
    setAttachments((prev) => {
      const target = prev.find((a) => a.id === id);
      if (target) {
        cleanup(target);
      }
      return prev.filter((a) => a.id !== id);
    });
  };

  const submit = async (e?: React.FormEvent) => {
    e?.preventDefault();
    const text = value.trim();
    if (!text && attachments.length === 0) {
      return;
    }

    // Snapshot the staged attachments and clear the composer state
    // immediately so the user can start typing the next message while
    // the encode/send is in flight.
    const pending = attachments;
    setAttachments([]);
    setValue('');

    let blocks: PromptBlock[] = [];
    try {
      blocks = await Promise.all(pending.map(attachmentToBlock));
    } catch (err) {
      setNotice(`Failed to read attachment: ${err instanceof Error ? err.message : String(err)}`);
      for (const att of pending) {
        cleanup(att);
      }
      return;
    }
    onSubmit(text, blocks);
    for (const att of pending) {
      cleanup(att);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (slashRef.current?.onKeyDown(e)) {
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault();
      void submit();
    }
  };

  const handlePaste = (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    if (!canAttachAnything) {
      return;
    }
    const pasted: File[] = [];
    for (const item of e.clipboardData.items) {
      if (item.kind === 'file') {
        const f = item.getAsFile();
        if (f) {
          pasted.push(f);
        }
      }
    }
    if (pasted.length > 0) {
      e.preventDefault();
      stageFiles(pasted);
    }
  };

  const handleDrop = (e: React.DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    setDragOver(false);
    if (!canAttachAnything || disabled) {
      return;
    }
    if (e.dataTransfer.files.length > 0) {
      stageFiles(e.dataTransfer.files);
    }
  };

  const handleDragOver = (e: React.DragEvent<HTMLDivElement>) => {
    // Accept-indicator only. Presence of files in dataTransfer is
    // unreliable during dragover in some browsers, so we show the
    // hint for any drag that enters the card.
    if (!canAttachAnything || disabled) {
      return;
    }
    e.preventDefault();
    setDragOver(true);
  };

  const handleDragLeave = () => setDragOver(false);

  const handleFilePicker = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files.length > 0) {
      stageFiles(e.target.files);
      // Allow the same file to be picked again later by resetting the
      // input; otherwise `onchange` will not fire for an identical name.
      e.target.value = '';
    }
  };

  const handleCommit = (next: string) => {
    setValue(next);
    textareaRef.current?.focus();
  };

  return (
    <form
      onSubmit={submit}
      className={cn(
        // Absolute so the log pane behind it keeps using the full
        // viewport height. Insets leave a visible gutter of scrollback
        // around the card so you can see the chat peeking out.
        // Bottom offset picks up `--mz-kb-inset` so the composer
        // rides above the virtual keyboard on mobile, and
        // `--mz-safe-bottom` so it clears the home indicator on iOS.
        // Both custom properties default to 0 on desktop, so the
        // composer rests at `bottom: 0.75rem` as before.
        'pointer-events-none absolute inset-x-3 z-10'
      )}
      style={{
        bottom: 'calc(0.75rem + var(--mz-kb-inset) + var(--mz-safe-bottom))'
      }}
    >
      <div
        onDrop={handleDrop}
        onDragOver={handleDragOver}
        onDragLeave={handleDragLeave}
        className={cn(
          // The card itself: takes pointer events, floats, and blurs
          // whatever log content slides under it. Border tint matches
          // the send button's primary accent so the card reads as a
          // first-class control, not a stray panel.
          'pointer-events-auto relative rounded-xl border border-[color:var(--primary)]/60 bg-background/70 shadow-lg shadow-black/30 backdrop-blur-md',
          dragOver && 'ring-2 ring-[color:var(--primary)]/70'
        )}
      >
        <SlashAutocomplete
          ref={slashRef}
          value={value}
          commands={session?.commands ?? []}
          prompts={session?.prompts ?? []}
          onCommit={handleCommit}
        />

        {attachments.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2.5">
            {attachments.map((att) => (
              <AttachmentChip key={att.id} attachment={att} onRemove={() => removeAttachment(att.id)} />
            ))}
          </div>
        )}

        <Textarea
          ref={textareaRef}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={handleKeyDown}
          onPaste={handlePaste}
          disabled={!session}
          readOnly={busy}
          placeholder={busy ? 'The agent is working...' : 'Message... (Enter to send, Shift+Enter for newline)'}
          rows={minRows}
          autoFocus
          className={cn(
            // Keep text clear of the overlay widgets on the right
            // (desktop send button) and along the bottom row. The
            // bottom row is taller on mobile (44 px send button), so
            // the textarea reserves more bottom padding there.
            // 16 px (`text-base`) on mobile prevents iOS Safari
            // auto-zooming on focus; desktop keeps the denser 14 px.
            'border-0 bg-transparent shadow-none pr-3 md:pr-14 pl-3 pt-3 pb-16 md:pb-12 text-base md:text-sm',
            'focus-visible:ring-0 focus-visible:ring-offset-0'
          )}
        />

        {/* Top-right: send (desktop only). On mobile the send button
         * moves to the bottom row so it is within thumb reach. */}
        <div className="absolute right-2 top-2 hidden md:block">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="submit"
                size="icon"
                className="size-9 touch-manipulation"
                disabled={!canSend}
                aria-label="Send message"
              >
                <SendIcon className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="left">Send</TooltipContent>
          </Tooltip>
        </div>

        {/* Bottom row.
         *   Desktop: [cwd] [attach]            [Agent] [Model]
         *   Mobile:  [cwd] [attach] [settings]          [send]
         * The settings trigger only shows when the agent advertises at
         * least one mode or model (`ModeModelSelectors` would otherwise
         * render nothing). */}
        <div className="absolute inset-x-2 bottom-2 flex items-center justify-between gap-2">
          <div className="flex items-center gap-1.5">
            <CwdChip session={session} />
            {canAttachAnything && (
              <>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="size-7 touch:size-11 text-[color:var(--primary)] touch-manipulation"
                      disabled={disabled}
                      onClick={() => fileInputRef.current?.click()}
                      aria-label="Attach file"
                    >
                      <PaperclipIcon className="size-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">Attach a file</TooltipContent>
                </Tooltip>
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  hidden
                  accept={acceptAttr}
                  onChange={handleFilePicker}
                />
              </>
            )}
            <MobileSettingsTrigger session={session} />
          </div>
          <div className="flex items-center gap-1.5">
            {/* Inline Agent/Model pickers on desktop only. */}
            <div className="hidden md:block">
              <ModeModelSelectors session={session} layout="row" />
            </div>
            {/* Mobile send button, matching the desktop one's disabled
             * state but positioned in the bottom-right where the thumb
             * can reach it. */}
            <Button
              type="submit"
              size="icon"
              className="size-11 md:hidden touch-manipulation"
              disabled={!canSend}
              aria-label="Send message"
            >
              <SendIcon className="size-4" />
            </Button>
          </div>
        </div>

        {notice && (
          <div
            role="alert"
            className="absolute -top-8 left-3 right-3 truncate rounded-md border border-[color:var(--attn-error)]/40 bg-[color:var(--attn-error)]/15 px-2.5 py-1 text-xs text-[color:var(--attn-error)]"
          >
            {notice}
          </div>
        )}

        {dragOver && (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center rounded-xl bg-background/60 text-xs font-medium text-[color:var(--primary)]">
            Drop to attach
          </div>
        )}
      </div>
    </form>
  );
};

type ChipProps = {
  attachment: Attachment;
  onRemove: () => void;
};

const AttachmentChip = ({ attachment, onRemove }: ChipProps) => {
  const sizeLabel = formatBytes(attachment.size);
  return (
    <div className="inline-flex items-center gap-1.5 rounded-md border border-border bg-card px-1.5 py-1 text-[11px] text-foreground">
      {attachment.previewUrl ? (
        <img src={attachment.previewUrl} alt="" className="h-6 w-6 rounded-sm object-cover" />
      ) : (
        <span className="inline-flex h-6 w-6 items-center justify-center rounded-sm bg-muted text-[10px] uppercase text-muted-foreground">
          {kindLabel(attachment.kind)}
        </span>
      )}
      <span className="max-w-[12rem] truncate" title={attachment.name}>
        {attachment.name}
      </span>
      <span className="text-muted-foreground">{sizeLabel}</span>
      <button
        type="button"
        aria-label={`Remove ${attachment.name}`}
        className="flex size-4 items-center justify-center rounded-sm text-muted-foreground hover:text-foreground"
        onClick={onRemove}
      >
        <XIcon className="size-3" />
      </button>
    </div>
  );
};

const kindLabel = (kind: Attachment['kind']): string => {
  switch (kind) {
    case 'image':
      return 'img';
    case 'text-resource':
      return 'txt';
    case 'binary-resource':
      return 'bin';
  }
};

const formatBytes = (bytes: number): string => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
};

// Mobile-only trigger that opens a DropdownMenu containing the Agent
// and Model pickers stacked vertically. Reuses the existing
// `@radix-ui/react-dropdown-menu` primitive so we don't pull in a
// new dep. Renders nothing when the agent advertised no modes and no
// models (e.g., non-Kiro agents); matches `ModeModelSelectors`'s own
// empty-state rule.
const MobileSettingsTrigger = ({ session }: { session: Session | null }) => {
  if (!session) {
    return null;
  }
  if (session.modes.length === 0 && session.models.length === 0) {
    return null;
  }
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-11 md:hidden touch-manipulation"
          aria-label="Session settings"
        >
          <SettingsIcon className="size-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        side="top"
        align="start"
        // Radix applies its own min-width inside DropdownMenuContent;
        // constrain so the Agent/Model stack (each ~10rem) fits the
        // popover comfortably on a 320 px viewport.
        className="p-2"
      >
        <ModeModelSelectors session={session} layout="stack" />
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
