// CONST-04: Minimal login page, adapted from harmony-web's Login/LoginModal. Session-cookie
// auth only — no API key field, nothing written to browser storage.
import { useState } from 'react';

interface LoginProps {
  onLogin: (username: string, password: string) => Promise<unknown>;
}

export function Login({ onLogin }: LoginProps) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await onLogin(username, password);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div style={{
      height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: 'var(--bg-base)',
    }}>
      <form onSubmit={handleSubmit} style={{
        width: 320, padding: 'var(--space-6)', background: 'var(--bg-surface)',
        border: '1px solid var(--border-subtle)', borderRadius: 'var(--radius-lg)',
        display: 'flex', flexDirection: 'column', gap: 'var(--space-3)',
      }}>
        <div style={{ color: 'var(--accent-primary)', fontWeight: 700, fontSize: 20 }}>Constellation</div>
        <p style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)', margin: 0 }}>
          Sign in to the control plane.
        </p>
        <input
          type="text"
          placeholder="Username"
          value={username}
          onChange={e => setUsername(e.target.value)}
          style={inputStyle}
          autoComplete="username"
        />
        <input
          type="password"
          placeholder="Password"
          value={password}
          onChange={e => setPassword(e.target.value)}
          style={inputStyle}
          autoComplete="current-password"
        />
        {error && <div style={{ color: 'var(--status-error)', fontSize: 'var(--text-sm)' }}>{error}</div>}
        <button type="submit" disabled={submitting} className="h-btn h-btn-teal">
          {submitting ? 'Signing in…' : 'Sign in'}
        </button>
      </form>
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  padding: 'var(--space-2) var(--space-3)',
  borderRadius: 'var(--radius-md)',
  border: '1px solid var(--border-default)',
  background: 'var(--bg-surface-raised)',
  color: 'var(--text-primary)',
  fontSize: 'var(--text-base)',
};
