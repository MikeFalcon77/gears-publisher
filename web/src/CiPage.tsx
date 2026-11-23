import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { GitPullRequest, ListOrdered, Play, X } from 'lucide-react';
import { api } from './api';
import { navigate } from './router';
import { jobCounts, parsePr, prNumber, queuePlace } from './ciview';
import { formatDuration, formatTime, isActive, statusLabel, tone } from './runview';
import type { CiInfo, CiSummary, QueuedCi, Started } from './types';
import { Badge, Button, ErrorNote } from './ui';

export function CiPage({ ci }: { ci: CiInfo }) {
  const client = useQueryClient();
  const runs = useQuery({
    queryKey: ['runs', 'ci'],
    queryFn: () => api<CiSummary[]>('/runs?kind=ci'),
    refetchInterval: 5000,
  });
  const queue = useQuery({
    queryKey: ['ci-queue'],
    queryFn: () => api<QueuedCi[]>('/ci/queue'),
    refetchInterval: 5000,
  });
  const [text, setText] = useState('');
  const [queued, setQueued] = useState<{ pr: number; position: number }>();
  const pr = parsePr(text);
  const refresh = () => {
    void client.invalidateQueries({ queryKey: ['runs', 'ci'] });
    void client.invalidateQueries({ queryKey: ['ci-queue'] });
  };
  const start = useMutation({
    mutationFn: (number: number) => api<Started>('/ci', 'POST', { pr: number }),
    onSuccess: (started, number) => {
      refresh();
      if (started.queued) {
        setQueued({ pr: number, position: started.position ?? 1 });
        setText('');
      } else {
        navigate(`/runs/${started.id}`);
      }
    },
  });
  const cancelQueued = useMutation({
    mutationFn: (id: string) => api<{ ok: boolean }>(`/ci/queue/${id}/cancel`, 'POST'),
    onSuccess: refresh,
  });
  // Only runs and entries this person may see; someone else's run may still be in
  // progress, and the server then queues the request.
  const active = runs.data?.find((r) => isActive(r.progress));
  const waiting = queue.data ?? [];
  const supersedes =
    pr !== undefined &&
    ((active !== undefined && prNumber(active.record?.facts) === pr) || waiting.some((q) => q.pr === pr));
  const queues = !supersedes && (active !== undefined || waiting.length > 0);

  return (
    <div className="space-y-4">
      <section className="flex flex-wrap items-end gap-3 rounded-xl bg-white p-4 shadow-sm ring-1 ring-slate-200">
        <div className="mr-auto max-w-3xl">
          <h1 className="text-base font-semibold">Run CI for a pull request</h1>
          <p className="text-sm text-slate-600">
            Runs the <code className="font-mono text-xs">pull_request</code> jobs of gears-rust’s{' '}
            <code className="font-mono text-xs">ci.yml</code>, <code className="font-mono text-xs">fmt.yml</code> and{' '}
            <code className="font-mono text-xs">test-suite.yml</code> on{' '}
            <code className="font-mono text-xs">refs/pull/N/merge</code>, skipping jobs the workflows’ path filters
            skip. A failed job does not stop the others.
          </p>
          <p className="mt-0.5 text-xs text-slate-500">
            Not run here: Docker lanes (database integration tests, e2e), macOS/Windows, FIPS, coverage, CodeQL. Results
            are not posted to GitHub.
          </p>
        </div>
        <form
          className="flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (pr !== undefined) start.mutate(pr);
          }}
        >
          <label className="relative">
            <span className="sr-only">Pull request number or URL</span>
            <GitPullRequest className="pointer-events-none absolute top-1/2 left-2 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <input
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder="PR number or URL"
              inputMode="numeric"
              className="w-56 rounded-md py-1.5 pr-2 pl-8 text-sm ring-1 ring-slate-300 ring-inset focus:ring-2 focus:ring-sky-500 focus:outline-none"
            />
          </label>
          <Button
            type="submit"
            variant="primary"
            disabled={pr === undefined || start.isPending || Boolean(ci.unavailable)}
            title={
              ci.unavailable ??
              (supersedes
                ? 'Replaces the run or request for this PR, like a new push on GitHub'
                : queues
                  ? 'Waits for the CI in progress, then starts by itself'
                  : 'Starts now, or waits for the CI in progress, then starts by itself')
            }
          >
            <Play className="h-4 w-4" /> {supersedes ? 'Run again' : queues ? 'Queue CI' : 'Run CI'}
          </Button>
        </form>
      </section>
      {ci.unavailable && <ErrorNote error={`PR CI is disabled on this host: ${ci.unavailable}`} />}
      {queued && !start.error && (
        <p role="status" className="rounded-md bg-sky-50 px-3 py-2 text-sm text-sky-900 ring-1 ring-sky-200">
          PR #{queued.pr} is queued: {queuePlace(queued.position).toLowerCase()}. It starts by itself; this page updates.
        </p>
      )}
      <ErrorNote error={start.error} />
      <ErrorNote error={cancelQueued.error} />
      <ErrorNote error={runs.error} />
      <ErrorNote error={queue.error} />

      {waiting.length > 0 && (
        <section className="overflow-x-auto rounded-xl bg-white shadow-sm ring-1 ring-slate-200">
          <h2 className="flex items-center gap-2 px-4 pt-3 text-sm font-semibold">
            <ListOrdered className="h-4 w-4 text-slate-400" /> Queued
          </h2>
          <table className="w-full text-left text-sm">
            <thead className="text-xs tracking-wide text-slate-500 uppercase">
              <tr>
                <th className="px-4 py-2 font-medium">Pull request</th>
                <th className="px-4 py-2 font-medium">Place</th>
                <th className="px-4 py-2 font-medium">Queued</th>
                <th className="px-4 py-2 font-medium">By</th>
                <th className="px-4 py-2" />
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-100">
              {waiting.map((entry) => (
                <tr key={entry.id}>
                  <td className="px-4 py-2.5">
                    <span className="font-medium">#{entry.pr}</span>
                    {entry.jobs?.length ? (
                      <span className="font-mono text-xs text-slate-400"> · re-run: {entry.jobs.join(', ')}</span>
                    ) : null}
                  </td>
                  <td className="px-4 py-2.5 text-slate-600">{queuePlace(entry.position)}</td>
                  <td className="px-4 py-2.5 whitespace-nowrap text-slate-600">{formatTime(entry.queued_at)}</td>
                  <td className="px-4 py-2.5 text-slate-600">{entry.requested_by}</td>
                  <td className="px-4 py-2.5 text-right">
                    <Button
                      onClick={() => cancelQueued.mutate(entry.id)}
                      disabled={cancelQueued.isPending}
                      title="Take it out of the queue"
                    >
                      <X className="h-4 w-4" /> Cancel
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}

      <section className="overflow-x-auto rounded-xl bg-white shadow-sm ring-1 ring-slate-200">
        <table className="w-full text-left text-sm">
          <thead className="bg-slate-50 text-xs tracking-wide text-slate-500 uppercase">
            <tr>
              <th className="px-4 py-2 font-medium">Pull request</th>
              <th className="px-4 py-2 font-medium">Status</th>
              <th className="px-4 py-2 font-medium">Jobs</th>
              <th className="px-4 py-2 font-medium">Commit</th>
              <th className="px-4 py-2 font-medium">Started</th>
              <th className="px-4 py-2 font-medium">Duration</th>
              <th className="px-4 py-2 font-medium">By</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-slate-100">
            {runs.data?.map((run) => (
              <CiRow key={run.progress.id} run={run} />
            ))}
            {runs.data?.length === 0 && (
              <tr>
                <td colSpan={7} className="px-4 py-8 text-center text-slate-500">
                  No PR CI runs yet. Enter a PR number above.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </section>
    </div>
  );
}

function CiRow({ run }: { run: CiSummary }) {
  const facts = run.record?.facts;
  const number = prNumber(facts);
  const counts = jobCounts(facts);
  return (
    <tr className="cursor-pointer hover:bg-slate-50" onClick={() => navigate(`/runs/${run.progress.id}`)}>
      <td className="max-w-md px-4 py-2.5">
        <div className="truncate font-medium">
          {number !== undefined ? `#${number}` : 'PR'} {facts?.pr?.title ?? ''}
        </div>
        <div className="truncate font-mono text-xs text-slate-400">
          {facts?.pr ? `${facts.pr.head_ref} → ${facts.pr.base_ref}` : run.progress.id.slice(0, 8)}
          {facts?.only?.length ? ` · re-run: ${facts.only.join(', ')}` : ''}
        </div>
      </td>
      <td className="px-4 py-2.5">
        <Badge tone={tone(run.progress.state)}>{statusLabel(run.progress.state)}</Badge>
      </td>
      <td className="px-4 py-2.5 text-xs whitespace-nowrap tabular-nums">
        {counts.success + counts.failure + counts.skipped === 0 ? (
          <span className="text-slate-400">—</span>
        ) : (
          <span className="flex gap-2">
            <span className="text-emerald-700" title="passed">
              ✓ {counts.success}
            </span>
            {counts.failure > 0 && (
              <span className="font-semibold text-rose-700" title="failed">
                ✗ {counts.failure}
              </span>
            )}
            <span className="text-slate-400" title="skipped">
              – {counts.skipped}
            </span>
          </span>
        )}
      </td>
      <td className="px-4 py-2.5 font-mono text-xs text-slate-600">{facts?.merge_sha?.slice(0, 7) ?? '—'}</td>
      <td className="px-4 py-2.5 whitespace-nowrap text-slate-600">{formatTime(run.progress.created_at)}</td>
      <td className="px-4 py-2.5 text-slate-600">
        {formatDuration(run.progress.created_at, isActive(run.progress) ? undefined : run.progress.updated_at)}
      </td>
      <td className="px-4 py-2.5 text-slate-600">{run.record?.started_by ?? '—'}</td>
    </tr>
  );
}
