// Re-export hub for testing-library helpers.
//
// Test files live at `tests/ui/` (outside the package root), and Vite's
// bare-import resolver only walks parent directories from the importing
// file. To keep all tests in the umbrella directory we centralise the
// `@testing-library/*` imports here, inside `ui/src/`, where Vite's
// resolution already works. Test files then `import { render } from
// '@/__test_utils'`.
//
// Production code does not import this module; tree-shaking keeps it
// out of the SPA bundle.

export { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
export { default as userEvent } from '@testing-library/user-event';
