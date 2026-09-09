import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { login } from '@/lib/auth';

// The login form: everything the app renders while nobody is signed in.
//
// A 401 shows one fixed line whichever half was wrong, as the server
// answers it; a 429 shows the wait the `Retry-After` header names. A
// success flips the auth state and the app takes over.

export const LoginGate = () => {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (busy) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await login(username, password);
      if (result.kind === 'invalid') {
        setError('Wrong username or password.');
      } else if (result.kind === 'limited') {
        setError(`Too many attempts. Try again in ${result.seconds} seconds.`);
      }
      // `ok` unmounts the gate; nothing to do here.
    } catch {
      setError('The server could not be reached. Try again.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex h-[100dvh] items-center justify-center bg-background">
      <form
        onSubmit={submit}
        className="flex w-full max-w-xs flex-col gap-4 rounded-2xl border border-[color:var(--outline-variant)] bg-card p-8 shadow-[0_5px_10px_rgba(201,103,54,0.35)]"
      >
        <span
          className="self-center text-[2rem] tracking-wide text-[color:var(--primary)] select-none"
          style={{ fontFamily: 'var(--font-display)' }}
        >
          MEZAME
        </span>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="login-username">Username</Label>
          <Input
            id="login-username"
            autoComplete="username"
            autoFocus
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="login-password">Password</Label>
          <Input
            id="login-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        {error !== null && (
          <p role="alert" className="text-sm text-[color:var(--attn-error)]">
            {error}
          </p>
        )}
        <Button type="submit" disabled={busy || !username || !password}>
          Sign in
        </Button>
      </form>
    </div>
  );
};
