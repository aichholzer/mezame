// Vitest setup file. Imported once before any test runs.
//
// Brings in `@testing-library/jest-dom` so component tests can use
// matchers like `toBeInTheDocument()` and `toHaveAttribute(...)` without
// a per-file import. The matcher set is automatically merged into
// vitest's `expect` via the side effect of this import.

import '@testing-library/jest-dom/vitest';
