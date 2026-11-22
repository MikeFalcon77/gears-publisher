import { useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { LogOut, Rocket } from 'lucide-react';
import { api, ApiError } from './api';
import { configured, login, logout, watchUser, type Identity } from './firebase';
import { navigate, useRoute } from './router';
import { RunPage } from './RunPage';
import { RunsPage } from './RunsPage';
import { CiPage } from './CiPage';
import type { Me } from './types';
import { Button, ErrorNote } from './ui';
import { FlaggedFooter, Quip, UnflaggedBadge } from './irony';

export default function App() {
  const [user, setUser] = useState<Identity | null | undefined>(undefined);
  useEffect(() => watchUser(setUser), []);

  if (user === undefined) return <Centered>Loading…</Centered>;
  if (!user) return <Login />;
  return <SignedIn user={user} />;
}

function SignedIn({ user }: { user: Identity }) {
  const me = useQuery({
    queryKey: ['me', user.uid],
    queryFn: () => api<Me>('/me'),
    retry: (count, error) => !(error instanceof ApiError && [401, 403].includes(error.status)) && count < 2,
  });
  if (me.isPending) return <Centered>Loading…</Centered>;
  if (me.error instanceof ApiError && [401, 403].includes(me.error.status))
    return <AccessDenied user={user} />;
  if (me.error)
    return (
      <Centered>
        <ErrorNote error={me.error} />
      </Centered>
    );
  return <Shell me={me.data} />;
}

function Shell({ me }: { me: Me }) {
  const route = useRoute();
  return (
    <div className="flex min-h-full flex-col">
      <header className="border-b border-slate-200 bg-white">
        <div className="mx-auto flex max-w-[1600px] items-center gap-3 px-4 py-2.5">
          <a
            href="/"
            onClick={(e) => {
              e.preventDefault();
              navigate('/');
            }}
            className="flex items-center gap-2 font-semibold"
          >
            <Rocket className="h-5 w-5 text-sky-600" /> gears-publisher
          </a>
          <nav className="flex items-center gap-1 text-sm">
            <NavLink to="/" active={route.page === 'runs'}>
              Releases
            </NavLink>
            <NavLink to="/ci" active={route.page === 'ci'}>
              PR CI
            </NavLink>
          </nav>
          <span className="hidden text-sm text-slate-500 md:inline">constructorfabric/gears-rust</span>
          <UnflaggedBadge />
          <div className="ml-auto flex items-center gap-3 text-sm text-slate-600">
            <span>{me.email}</span>
            <Button onClick={() => void logout()} title="Sign out">
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </header>
      <main className="mx-auto flex w-full max-w-[1600px] flex-1 flex-col px-4 py-4">
        {route.page === 'run' ? (
          <RunPage id={route.id} me={me} />
        ) : route.page === 'ci' ? (
          <CiPage ci={me.ci} />
        ) : (
          <RunsPage steps={me.steps} />
        )}
      </main>
      <FlaggedFooter />
    </div>
  );
}

function NavLink({ to, active, children }: { to: string; active: boolean; children: React.ReactNode }) {
  return (
    <a
      href={to}
      aria-current={active ? 'page' : undefined}
      onClick={(e) => {
        e.preventDefault();
        navigate(to);
      }}
      className={`rounded-md px-2.5 py-1 font-medium ${active ? 'bg-slate-900 text-white' : 'text-slate-600 hover:bg-slate-100 hover:text-slate-900'}`}
    >
      {children}
    </a>
  );
}

function Login() {
  const [error, setError] = useState<unknown>(null);
  return (
    <Centered>
      <div className="w-full max-w-sm space-y-4 rounded-xl bg-white p-6 shadow-sm ring-1 ring-slate-200">
        <h1 className="flex items-center gap-2 text-lg font-semibold">
          <Rocket className="h-5 w-5 text-sky-600" /> gears-publisher
        </h1>
        <p className="text-sm text-slate-600">Releases of constructorfabric/gears-rust.</p>
        <div className="space-y-1">
          <Quip>
            GitHub flagged our organization, so we built our own GitHub Actions. With a button. And Google
            sign-in.
          </Quip>
          <p className="font-mono text-xs text-slate-400">
            <s>.github/workflows/release-plz.yml</s> → this page
          </p>
        </div>
        {!configured && <ErrorNote error="Firebase is not configured in this build." />}
        <Button
          variant="primary"
          className="w-full"
          disabled={!configured}
          onClick={() => login().catch(setError)}
        >
          Sign in with Google
        </Button>
        <ErrorNote error={error} />
      </div>
    </Centered>
  );
}

/** Shows what an administrator needs to add this account to the allowlist. */
function AccessDenied({ user }: { user: Identity }) {
  const entry = `- uid: "${user.uid}"\n  email: "${user.email}"\n  tenant_id: "67656172-732d-7075-626c-697368657201"\n  token_scopes: ["publisher"]`;
  return (
    <Centered>
      <div className="w-full max-w-lg space-y-4 rounded-xl bg-white p-6 shadow-sm ring-1 ring-slate-200">
        <h1 className="text-lg font-semibold">Access denied</h1>
        <p className="text-sm text-slate-600">
          This Google account is not on the publisher's list. Send these details to an administrator;
          they add them to <code className="font-mono">firebase-authn-plugin.identities</code> in{' '}
          <code className="font-mono">config/publisher.yaml</code> and restart the service.
        </p>
        <Quip>Being turned away hurts. GitHub did it to our whole organization, so we get it.</Quip>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="text-slate-500">Email</dt>
          <dd className="font-mono break-all">{user.email || '(none)'}</dd>
          <dt className="text-slate-500">UID</dt>
          <dd className="font-mono break-all">{user.uid}</dd>
        </dl>
        {!user.emailVerified && (
          <ErrorNote error="This account's email address is not verified; it cannot be admitted." />
        )}
        <pre className="overflow-x-auto rounded-md bg-slate-900 p-3 text-xs text-slate-100">{entry}</pre>
        <div className="flex gap-2">
          <Button onClick={() => void navigator.clipboard?.writeText(entry)}>Copy entry</Button>
          <Button onClick={() => void logout()}>Use another account</Button>
        </div>
      </div>
    </Centered>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return <div className="flex min-h-full items-center justify-center p-4 text-slate-600">{children}</div>;
}
