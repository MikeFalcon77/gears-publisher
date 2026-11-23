import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Check, X } from 'lucide-react';
import { api } from './api';
import { byStatus, providerLabel, who } from './access';
import { formatTime } from './runview';
import type { AccessList, UserEntry } from './types';
import { Badge, Button, ErrorNote } from './ui';

type Decision = 'approve' | 'reject';

/** Access requests: approve or reject them; revoke members; re-admit declined people. */
export function AdminPage() {
  const client = useQueryClient();
  const list = useQuery({
    queryKey: ['access'],
    queryFn: () => api<AccessList>('/admin/users'),
    refetchInterval: 15_000,
  });
  const decide = useMutation({
    mutationFn: ({ user, decision }: { user: UserEntry; decision: Decision }) =>
      api<UserEntry>(`/admin/users/${user.subject_id}/${decision}`, 'POST'),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ['access'] });
      void client.invalidateQueries({ queryKey: ['me'] });
    },
  });
  const groups = byStatus(list.data?.users ?? []);
  const busy = decide.isPending;
  const act = (user: UserEntry, decision: Decision) => decide.mutate({ user, decision });

  return (
    <div className="space-y-4">
      <ErrorNote error={list.error} />
      <ErrorNote error={decide.error} />
      <Section
        title="Waiting for a decision"
        empty="Nobody is waiting."
        users={groups.pending}
        actions={(user) => (
          <>
            <Button variant="primary" disabled={busy} onClick={() => act(user, 'approve')}>
              <Check className="h-4 w-4" /> Approve
            </Button>
            <Button disabled={busy} onClick={() => act(user, 'reject')}>
              <X className="h-4 w-4" /> Reject
            </Button>
          </>
        )}
      />
      <Section
        title="Members (PR CI, releases read-only)"
        empty="No members yet."
        users={groups.approved}
        actions={(user) => (
          <Button variant="danger" disabled={busy} onClick={() => act(user, 'reject')}>
            Revoke
          </Button>
        )}
      />
      <Section
        title="Declined"
        empty="Nobody was declined."
        users={groups.rejected}
        actions={(user) => (
          <Button disabled={busy} onClick={() => act(user, 'approve')}>
            Approve
          </Button>
        )}
      />
      <section className="space-y-2 rounded-xl bg-white p-4 shadow-sm ring-1 ring-slate-200">
        <h2 className="text-sm font-semibold">Admins</h2>
        <p className="text-sm text-slate-600">
          Configured in <code className="font-mono">publisher.admins</code> (publisher.yaml); not changed
          here.
        </p>
        <ul className="text-sm">
          {list.data?.admins.map((admin) => (
            <li key={admin.uid} className="flex gap-3">
              <span>{admin.email ?? '—'}</span>
              <span className="font-mono text-xs text-slate-400">{admin.uid}</span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

function Section({
  title,
  empty,
  users,
  actions,
}: {
  title: string;
  empty: string;
  users: UserEntry[];
  actions: (user: UserEntry) => React.ReactNode;
}) {
  return (
    <section className="overflow-hidden rounded-xl bg-white shadow-sm ring-1 ring-slate-200">
      <h2 className="flex items-center gap-2 px-4 pt-3 pb-2 text-sm font-semibold">
        {title} <Badge tone="neutral">{users.length}</Badge>
      </h2>
      <div className="overflow-x-auto">
        <table className="w-full text-left text-sm">
          <thead className="bg-slate-50 text-xs tracking-wide text-slate-500 uppercase">
            <tr>
              <th className="px-4 py-2 font-medium">Who</th>
              <th className="px-4 py-2 font-medium">Note</th>
              <th className="px-4 py-2 font-medium">Asked</th>
              <th className="px-4 py-2 font-medium">Decided</th>
              <th className="px-4 py-2 font-medium" />
            </tr>
          </thead>
          <tbody className="divide-y divide-slate-100">
            {users.map((user) => (
              <tr key={user.subject_id} className="align-top">
                <td className="px-4 py-2.5">
                  <div className="font-medium">{who(user)}</div>
                  <div className="text-xs text-slate-500">
                    {user.email && user.email !== who(user) && <span>{user.email} · </span>}
                    {providerLabel(user.provider)}
                  </div>
                  <div className="font-mono text-xs text-slate-400">{user.uid}</div>
                </td>
                <td className="max-w-md px-4 py-2.5 whitespace-pre-wrap text-slate-700">{user.note}</td>
                <td className="px-4 py-2.5 text-slate-600">{formatTime(user.requested_at)}</td>
                <td className="px-4 py-2.5 text-slate-600">
                  {user.decided_at ? (
                    <>
                      {formatTime(user.decided_at)}
                      <div className="text-xs text-slate-500">{user.decided_by}</div>
                    </>
                  ) : (
                    '—'
                  )}
                </td>
                <td className="px-4 py-2.5">
                  <div className="flex justify-end gap-2">{actions(user)}</div>
                </td>
              </tr>
            ))}
            {users.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-6 text-center text-slate-500">
                  {empty}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}
