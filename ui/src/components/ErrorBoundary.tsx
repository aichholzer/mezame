import { Component, type ErrorInfo, type ReactNode } from 'react';

// The one place a render error lands. Without a boundary React unmounts
// the whole tree and the page goes blank with the cause only in the
// console; an entry from the shared, unauthenticated `state.json` used to
// be able to do that on every load. Reload is the only action offered. A
// "reset saved tabs" button was considered and dropped: every other open
// browser writes its own tab list straight back through the sync before
// the reload lands, so the button could not keep its promise.

type Props = {
  children: ReactNode;
  /** What "Reload" does; the page reload by default. Injected in tests. */
  reload?: () => void;
};

type State = {
  error: Error | null;
};

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // The console is where the component stack is useful; the page
    // shows the message.
    console.error('Mezame could not draw the page', error, info.componentStack);
  }

  render(): ReactNode {
    const { error } = this.state;
    if (!error) {
      return this.props.children;
    }
    const reload = this.props.reload ?? (() => window.location.reload());
    return (
      <main
        role="alert"
        className="mx-auto flex min-h-dvh max-w-xl flex-col justify-center gap-4 p-6 text-foreground"
      >
        <h1 className="text-lg font-semibold">Mezame could not draw the page</h1>
        <pre className="overflow-x-auto rounded-md border border-border bg-muted p-3 text-xs">
          {error.message}
        </pre>
        <p className="text-sm text-muted-foreground">
          Reloading usually clears it. If it comes back on every load, the saved tab list on
          the server may hold an entry this build cannot draw.
        </p>
        <div>
          <button
            type="button"
            onClick={reload}
            className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-muted"
          >
            Reload
          </button>
        </div>
      </main>
    );
  }
}
