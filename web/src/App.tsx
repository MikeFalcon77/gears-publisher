import { useEffect, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { LogOut, Rocket } from 'lucide-react';
import { api, ApiError } from './api';
import { configured, login, logout, watchUser, type Identity, type Provider } from './firebase';
import { navigate, useRoute } from './router';
import { RunPage } from './RunPage';
import { RunsPage } from './RunsPage';
import { CiPage } from './CiPage';
import { AdminPage } from './AdminPage';
import { MAX_NOTE } from './access';
import type { Me, UserEntry } from './types';
import { Badge, Button, ErrorNote } from './ui';
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
    // A pending person sees the approval arrive; an admin sees new requests.
    refetchInterval: (query) =>
      query.state.data?.role === 'pending' ? 15_000 : query.state.data?.role === 'admin' ? 30_000 : false,
    retry: (count, error) => !(error instanceof ApiError && [401, 403].includes(error.status)) && count < 2,
  });
  if (me.isPending) return <Centered>Loading…</Centered>;
  if (me.error)
    return (
      <Centered>
        <div className="w-full max-w-sm space-y-3">
          <ErrorNote error={me.error} />
          <Button onClick={() => void logout()}>Sign out</Button>
        </div>
      </Centered>
    );
  switch (me.data.role) {
    case 'admin':
    case 'member':
      return <Shell me={me.data} />;
    case 'rejected':
      return <Declined me={me.data} />;
    default:
      return <RequestAccess me={me.data} user={user} />;
  }
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
            {me.role === 'admin' && (
              <NavLink to="/admin" active={route.page === 'admin'}>
                Admin
                {Boolean(me.pending_requests) && (
                  <span className="ml-1.5 rounded-full bg-amber-400 px-1.5 text-xs text-slate-900">
                    {me.pending_requests}
                  </span>
                )}
              </NavLink>
            )}
          </nav>
          <span className="hidden text-sm text-slate-500 md:inline">constructorfabric/gears-rust</span>
          <UnflaggedBadge />
          <div className="ml-auto flex items-center gap-3 text-sm text-slate-600">
            {me.role === 'member' && <Badge tone="neutral">PR CI only</Badge>}
            <span>{me.email ?? me.name ?? me.uid}</span>
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
        ) : route.page === 'admin' && me.role === 'admin' ? (
          <AdminPage />
        ) : (
          <RunsPage steps={me.steps} canStart={me.role === 'admin'} />
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
            GitHub flagged our organization, so we built our own GitHub Actions. With a button. And GitHub
            sign-in, because irony.
          </Quip>
          <p className="font-mono text-xs text-slate-400">
            <s>.github/workflows/release-plz.yml</s> → this page
          </p>
        </div>
        {!configured && <ErrorNote error="Firebase is not configured in this build." />}
        {(['google', 'github'] as Provider[]).map((provider) => (
          <Button
            key={provider}
            variant={provider === 'google' ? 'primary' : 'secondary'}
            className="w-full"
            disabled={!configured}
            onClick={() => login(provider).catch(setError)}
          >
            Sign in with {provider === 'google' ? 'Google' : 'GitHub'}
          </Button>
        ))}
        <p className="text-xs text-slate-500">New here? Sign in, then ask for access; an admin approves it.</p>
        <ErrorNote error={error} />
      </div>
    </Centered>
  );
}

/** Signed in, not a member: ask for access, or wait for an admin to decide. */
function RequestAccess({ me, user }: { me: Me; user: Identity }) {
  const client = useQueryClient();
  const pending = me.role === 'pending';
  const [note, setNote] = useState(me.note ?? user.displayName);
  const [editing, setEditing] = useState(!pending);
  const send = useMutation({
    mutationFn: () => api<UserEntry>('/access-request', 'POST', { note }),
    onSuccess: (request) => {
      // Shown at once; the refetch confirms it.
      client.setQueryData<Me>(['me', user.uid], { ...me, role: 'pending', note: request.note });
      setEditing(false);
      void client.invalidateQueries({ queryKey: ['me'] });
    },
  });
  // Characters as the server counts them (code points, not UTF-16 units).
  const length = [...note.trim()].length;
  const tooLong = length > MAX_NOTE;
  return (
    <Centered>
      <div className="w-full max-w-lg space-y-4 rounded-xl bg-white p-6 shadow-sm ring-1 ring-slate-200">
        <h1 className="text-lg font-semibold">{pending ? 'Waiting for an admin' : 'Ask for access'}</h1>
        <p className="text-sm text-slate-600">
          {pending
            ? 'Your request is with the admins. Once approved you can run PR CI and follow releases; this page updates by itself.'
            : 'Tell the admins who you are and why you need access. Once approved you can run PR CI and follow releases.'}
        </p>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="text-slate-500">Signed in as</dt>
          <dd className="break-all">{me.email ?? me.name ?? me.uid}</dd>
        </dl>
        {editing ? (
          <>
            <label className="block space-y-1 text-sm">
              <span className="text-slate-700">Name and why you need access</span>
              <textarea
                className="block min-h-24 w-full rounded-md border border-slate-300 px-3 py-2 text-sm focus:border-slate-500 focus:outline-none"
                value={note}
                placeholder="Вася Иванов, gears-rust contributor: run CI for my PRs"
                onChange={(e) => setNote(e.target.value)}
              />
              <span className={`text-xs ${tooLong ? 'text-rose-700' : 'text-slate-400'}`}>
                {length}/{MAX_NOTE}
              </span>
            </label>
            <div className="flex gap-2">
              <Button
                variant="primary"
                disabled={send.isPending || !note.trim() || tooLong}
                onClick={() => send.mutate()}
              >
                {pending ? 'Save' : 'Send request'}
              </Button>
              {pending && <Button onClick={() => setEditing(false)}>Back</Button>}
            </div>
          </>
        ) : (
          <>
            <blockquote className="rounded-md bg-slate-50 px-3 py-2 text-sm whitespace-pre-wrap text-slate-700 ring-1 ring-slate-200">
              {me.note}
            </blockquote>
            <Button onClick={() => setEditing(true)}>Edit note</Button>
          </>
        )}
        <ErrorNote error={send.error} />
        <Button onClick={() => void logout()}>Use another account</Button>
      </div>
    </Centered>
  );
}

function Declined({ me }: { me: Me }) {
  return (
    <Centered>
      <div className="w-full max-w-lg space-y-4 rounded-xl bg-white p-6 shadow-sm ring-1 ring-slate-200">
        <h1 className="text-lg font-semibold">Access declined</h1>
        <p className="text-sm text-slate-600">
          An admin declined access for {me.email ?? me.name ?? me.uid}. Talk to them if this is a mistake.
        </p>
        <Quip>Being turned away hurts. GitHub did it to our whole organization, so we get it.</Quip>
        <Button onClick={() => void logout()}>Use another account</Button>
      </div>
    </Centered>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return <div className="flex min-h-full items-center justify-center p-4 text-slate-600">{children}</div>;
}
