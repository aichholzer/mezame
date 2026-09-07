import type { PromptBlock, PromptCapabilities } from '@/types';

// Client-side attachment plumbing for the composer. Paste, drop, and
// upload-picker all funnel through `fileToAttachment` so the rest of
// the composer only deals with `Attachment` values.
//
// ## History replay caveat
//
// Attachments are not replayed on reconnect. `/history` carries the text
// of each turn (`EntryBody` in `src/backend.rs`, rendered by
// `renderHistoryText` in `useMezame.ts`); image and resource blocks go to
// the Backend and are not recorded, so after a reload a prior turn shows
// only its text.

export const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024; // 5 MB per file.
export const MAX_TOTAL_BYTES = 20 * 1024 * 1024; // 20 MB across all attachments.
export const MAX_ATTACHMENTS = 10;

/** What the composer tracks while a file is staged but not yet sent. */
export type Attachment = {
  /** Stable id for React keys and remove-by-id. */
  id: string;
  /** Display name shown on the chip. */
  name: string;
  /** Source mime type (the Backend sees this). */
  mimeType: string;
  /** Byte size before base64 encoding, for quota accounting. */
  size: number;
  /** Routing decision: which `PromptBlock` we will build on submit. */
  kind: 'image' | 'text-resource' | 'binary-resource';
  /** Object URL for thumbnail/preview. Revoked when the attachment is
   * removed or the composer submits. Only populated for images. */
  previewUrl: string | null;
  /** Raw file for deferred read-on-submit. */
  file: File;
};

/** Reasons `fileToAttachment` refused a file. Null means accepted. */
export type RejectReason =
  | { kind: 'too-large'; bytes: number; limit: number }
  | { kind: 'image-not-supported' }
  | { kind: 'embed-not-supported' };

export type StageResult =
  | { ok: true; attachment: Attachment }
  | { ok: false; reason: RejectReason };

const newId = (): string =>
  typeof crypto !== 'undefined' && 'randomUUID' in crypto ? crypto.randomUUID() : `att-${Math.random()}`;

const isTextish = (mime: string): boolean => {
  if (mime.startsWith('text/')) {
    return true;
  }
  // Common structured-text types that browsers sometimes label as
  // application/*.
  return [
    'application/json',
    'application/xml',
    'application/javascript',
    'application/x-sh',
    'application/x-yaml',
    'application/yaml'
  ].includes(mime);
};

/** Classify a file into a `PromptBlock` kind, gated by the capabilities
 * `ready` advertised. Returns either a staged attachment or the reason
 * we rejected it. No I/O happens here; the raw bytes are only read when
 * the user submits. */
export const fileToAttachment = (file: File, caps: PromptCapabilities): StageResult => {
  if (file.size > MAX_ATTACHMENT_BYTES) {
    return { ok: false, reason: { kind: 'too-large', bytes: file.size, limit: MAX_ATTACHMENT_BYTES } };
  }

  const mime = file.type || 'application/octet-stream';
  const isImage = mime.startsWith('image/');

  if (isImage) {
    if (!caps.image) {
      return { ok: false, reason: { kind: 'image-not-supported' } };
    }
    return {
      ok: true,
      attachment: {
        id: newId(),
        name: file.name,
        mimeType: mime,
        size: file.size,
        kind: 'image',
        previewUrl: URL.createObjectURL(file),
        file
      }
    };
  }

  if (isTextish(mime)) {
    if (!caps.embeddedContext) {
      return { ok: false, reason: { kind: 'embed-not-supported' } };
    }
    return {
      ok: true,
      attachment: {
        id: newId(),
        name: file.name,
        mimeType: mime,
        size: file.size,
        kind: 'text-resource',
        previewUrl: null,
        file
      }
    };
  }

  if (!caps.embeddedContext) {
    // Non-image, non-textish (e.g. application/pdf) only ever reaches
    // Mezame as a binary embedded resource. If embeddedContext is off,
    // the rejection is the same as for textish files: Mezame does not
    // accept embedded content. The previous "unknown-type" path was
    // misleading; the mime type was fine, Mezame just wouldn't take it.
    return { ok: false, reason: { kind: 'embed-not-supported' } };
  }

  return {
    ok: true,
    attachment: {
      id: newId(),
      name: file.name,
      mimeType: mime,
      size: file.size,
      kind: 'binary-resource',
      previewUrl: null,
      file
    }
  };
};

const readAsBase64 = (file: File): Promise<string> =>
  new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result;
      if (typeof result !== 'string') {
        reject(new Error('reader did not return a string'));
        return;
      }
      const comma = result.indexOf(',');
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.onerror = () => reject(reader.error ?? new Error('read failed'));
    reader.readAsDataURL(file);
  });

const readAsText = (file: File): Promise<string> => file.text();

/** Turn a staged attachment into a `PromptBlock`. Reads the file bytes
 * at submit time. Staging stays cheap for large files. */
export const attachmentToBlock = async (att: Attachment): Promise<PromptBlock> => {
  switch (att.kind) {
    case 'image': {
      const data = await readAsBase64(att.file);
      return { type: 'image', mimeType: att.mimeType, data };
    }
    case 'text-resource': {
      const text = await readAsText(att.file);
      return {
        type: 'resource',
        resource: {
          uri: `attachment://${encodeURIComponent(att.name)}`,
          mimeType: att.mimeType,
          text
        }
      };
    }
    case 'binary-resource': {
      const blob = await readAsBase64(att.file);
      return {
        type: 'resource',
        resource: {
          uri: `attachment://${encodeURIComponent(att.name)}`,
          mimeType: att.mimeType,
          blob
        }
      };
    }
  }
};

/** Human-readable description for a rejection reason. Used by the
 * composer's error toast. */
export const describeRejection = (reason: RejectReason): string => {
  switch (reason.kind) {
    case 'too-large':
      return `File is ${(reason.bytes / 1024 / 1024).toFixed(1)} MB, limit is ${reason.limit / 1024 / 1024} MB.`;
    case 'image-not-supported':
      return 'This session does not accept images.';
    case 'embed-not-supported':
      return 'This session does not accept embedded files.';
  }
};

/** Revokes the object URL behind a preview, if any. Call on remove
 * and on submit. */
export const cleanup = (att: Attachment): void => {
  if (att.previewUrl) {
    URL.revokeObjectURL(att.previewUrl);
  }
};
