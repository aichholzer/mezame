// Routing tests for `fileToAttachment` and friends. Builds real `File`
// objects via the constructor jsdom provides, drives the function, and
// asserts the StageResult shape.

import {
  attachmentToBlock,
  cleanup,
  describeRejection,
  fileToAttachment,
  MAX_ATTACHMENT_BYTES,
  type Attachment,
  type RejectReason
} from '@/lib/attachments';
import type { PromptCapabilities } from '@/types';

/** Wrap `fileToAttachment` and pull the attachment out, throwing if
 * the caller staged something the test expected to be accepted. */
function staged(file: File, caps: PromptCapabilities): Attachment {
  const result = fileToAttachment(file, caps);
  if (!result.ok) {
    throw new Error(`expected ok, got ${result.reason.kind}`);
  }
  return result.attachment;
}

function rejected(file: File, caps: PromptCapabilities): RejectReason {
  const result = fileToAttachment(file, caps);
  if (result.ok) {
    throw new Error(`expected rejection, got attachment kind=${result.attachment.kind}`);
  }
  return result.reason;
}

const png = (size = 8): File =>
  new File([new Uint8Array(size)], 'logo.png', { type: 'image/png' });
const txt = (size = 8): File =>
  new File(['hello'.repeat(size)], 'notes.txt', { type: 'text/plain' });
const pdf = (size = 8): File =>
  new File([new Uint8Array(size)], 'doc.pdf', { type: 'application/pdf' });
const json = (): File =>
  new File(['{}'], 'data.json', { type: 'application/json' });

// Mock URL.createObjectURL + revokeObjectURL: jsdom doesn't implement
// them by default, and they're called on every accepted image.
beforeAll(() => {
  if (!URL.createObjectURL) {
    URL.createObjectURL = () => 'blob:mock';
  }
  if (!URL.revokeObjectURL) {
    URL.revokeObjectURL = () => {};
  }
});

// ---------- images ----------

describe('fileToAttachment / images', () => {
  it('routes a PNG to image when caps.image is true', () => {
    const a = staged(png(), { image: true });
    expect(a.kind).toBe('image');
    expect(a.mimeType).toBe('image/png');
    expect(a.previewUrl).not.toBeNull();
  });

  it('rejects an image when caps.image is false', () => {
    const r = rejected(png(), {});
    expect(r.kind).toBe('image-not-supported');
  });
});

// ---------- text-resource ----------

describe('fileToAttachment / text resources', () => {
  it('routes text/plain to text-resource when caps.embeddedContext is true', () => {
    const a = staged(txt(), { embeddedContext: true });
    expect(a.kind).toBe('text-resource');
    expect(a.mimeType).toBe('text/plain');
    expect(a.previewUrl).toBeNull();
  });

  it('rejects text/plain when caps.embeddedContext is false', () => {
    const r = rejected(txt(), {});
    expect(r.kind).toBe('embed-not-supported');
  });

  it('treats application/json as textish', () => {
    const a = staged(json(), { embeddedContext: true });
    expect(a.kind).toBe('text-resource');
  });
});

// ---------- binary-resource ----------

describe('fileToAttachment / binary resources', () => {
  it('routes application/pdf to binary-resource when caps.embeddedContext is true', () => {
    const a = staged(pdf(), { embeddedContext: true });
    expect(a.kind).toBe('binary-resource');
    expect(a.mimeType).toBe('application/pdf');
    expect(a.previewUrl).toBeNull();
  });

  // KNOWN BUG (#31): when caps.embeddedContext is false and the file is
  // not an image and not in the small textish allowlist, the function
  // returns `{ kind: 'unknown-type' }` instead of the more accurate
  // `embed-not-supported`. The user sees "Unrecognised file type."
  // when the real cause is that the agent simply doesn't accept
  // embedded files. `it.fails` marks this as expected-to-fail; once
  // #31 ships the fix, removing `.fails` will require this test to
  // pass with the new wording.
  it.fails(
    'should reject application/pdf with embed-not-supported when caps.embeddedContext is false (#31)',
    () => {
      const r = rejected(pdf(), {});
      expect(r.kind).toBe('embed-not-supported');
    }
  );

  // Lock in the current behaviour so a refactor doesn't accidentally
  // change the user-facing message in either direction. Update or
  // remove this test alongside the #31 fix.
  it('currently returns unknown-type for non-image, non-textish files when embeddedContext is false (#31)', () => {
    const r = rejected(pdf(), {});
    expect(r.kind).toBe('unknown-type');
  });
});

// ---------- size cap ----------

describe('fileToAttachment / size cap', () => {
  it('rejects files larger than MAX_ATTACHMENT_BYTES', () => {
    const big = new File([new Uint8Array(MAX_ATTACHMENT_BYTES + 1)], 'big.png', {
      type: 'image/png'
    });
    const r = rejected(big, { image: true });
    expect(r.kind).toBe('too-large');
    if (r.kind === 'too-large') {
      expect(r.bytes).toBe(MAX_ATTACHMENT_BYTES + 1);
      expect(r.limit).toBe(MAX_ATTACHMENT_BYTES);
    }
  });

  it('accepts files exactly at the limit', () => {
    const exact = new File([new Uint8Array(MAX_ATTACHMENT_BYTES)], 'edge.png', {
      type: 'image/png'
    });
    const a = staged(exact, { image: true });
    expect(a.size).toBe(MAX_ATTACHMENT_BYTES);
  });
});

// ---------- describeRejection ----------

describe('describeRejection', () => {
  it('returns a non-empty, non-key string for every reason', () => {
    const cases: RejectReason[] = [
      { kind: 'too-large', bytes: 10 * 1024 * 1024, limit: 5 * 1024 * 1024 },
      { kind: 'image-not-supported' },
      { kind: 'embed-not-supported' },
      { kind: 'unknown-type' }
    ];
    for (const reason of cases) {
      const text = describeRejection(reason);
      expect(text.length).toBeGreaterThan(0);
      // Should not be the literal reason key; should be a sentence.
      expect(text).not.toBe(reason.kind);
    }
  });

  it('mentions both the size and the limit in the too-large message', () => {
    const text = describeRejection({
      kind: 'too-large',
      bytes: 6 * 1024 * 1024,
      limit: 5 * 1024 * 1024
    });
    expect(text).toMatch(/6\.0/);
    expect(text).toContain('5 MB');
  });
});

// ---------- attachmentToBlock ----------

describe('attachmentToBlock', () => {
  it('reads an image as base64 and emits an image block', async () => {
    const a = staged(png(4), { image: true });
    const block = await attachmentToBlock(a);
    expect(block.type).toBe('image');
    if (block.type === 'image') {
      expect(block.mimeType).toBe('image/png');
      expect(typeof block.data).toBe('string');
      // The data field is base64; the comma prefix from the data URL
      // must have been stripped.
      expect(block.data).not.toMatch(/^data:/);
    }
  });

  it('reads a text resource as text and emits a resource block', async () => {
    const a = staged(txt(2), { embeddedContext: true });
    const block = await attachmentToBlock(a);
    expect(block.type).toBe('resource');
    if (block.type === 'resource' && 'text' in block.resource) {
      expect(block.resource.text).toContain('hello');
      expect(block.resource.uri).toMatch(/^attachment:\/\//);
      expect(block.resource.mimeType).toBe('text/plain');
    } else {
      throw new Error('expected text resource');
    }
  });

  it('reads a binary resource as base64 and emits a resource block with blob', async () => {
    const a = staged(pdf(4), { embeddedContext: true });
    const block = await attachmentToBlock(a);
    expect(block.type).toBe('resource');
    if (block.type === 'resource' && 'blob' in block.resource) {
      expect(typeof block.resource.blob).toBe('string');
      expect(block.resource.mimeType).toBe('application/pdf');
    } else {
      throw new Error('expected binary resource');
    }
  });
});

// ---------- cleanup ----------

describe('cleanup', () => {
  it('revokes the preview URL when one was created', () => {
    const revokeSpy = vi.spyOn(URL, 'revokeObjectURL');
    const a = staged(png(), { image: true });
    cleanup(a);
    expect(revokeSpy).toHaveBeenCalledWith(a.previewUrl);
  });

  it('is a no-op for attachments without a preview', () => {
    const revokeSpy = vi.spyOn(URL, 'revokeObjectURL');
    const a = staged(txt(), { embeddedContext: true });
    cleanup(a);
    expect(revokeSpy).not.toHaveBeenCalled();
  });
});
