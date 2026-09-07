// The recovery screen a render error lands on. Without it React unmounts
// the whole tree and the page goes blank with the cause only in the
// console.

import { render, screen, userEvent } from '@/__test_utils';
import { ErrorBoundary } from '@/components/ErrorBoundary';

const Thrower = (): never => {
  throw new Error('boom');
};

describe('ErrorBoundary', () => {
  it('shows the message and a Reload action when a child throws', async () => {
    // React reports the caught error on the console; that is expected here.
    const quiet = vi.spyOn(console, 'error').mockImplementation(() => {});
    const reload = vi.fn();
    render(
      <ErrorBoundary reload={reload}>
        <Thrower />
      </ErrorBoundary>
    );
    expect(screen.getByRole('alert')).toHaveTextContent('Mezame could not draw the page');
    expect(screen.getByText('boom')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Reload' }));
    expect(reload).toHaveBeenCalledTimes(1);
    quiet.mockRestore();
  });

  it('renders its children when nothing throws', () => {
    render(
      <ErrorBoundary>
        <p>fine</p>
      </ErrorBoundary>
    );
    expect(screen.getByText('fine')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
