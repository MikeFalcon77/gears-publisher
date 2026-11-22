import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { GitPullRequest, Play } from 'lucide-react';
import { api } from './api';
import { navigate } from './router';
import { jobCounts, parsePr, prNumber } from './ciview';
import { formatDuration, formatTime, isActive, statusLabel, tone } from './runview';
import type { CiInfo, CiSummary } from './types';
import { Badge, Button, ErrorNote } from './ui';

export function CiPage({ ci }: { ci: CiInfo }) {
  const client = useQueryClient();
  const runs = useQuery({
    queryKey: ['runs', 'ci'],
    queryFn: () => api<CiSummary[]>('/runs?kind=ci'),
    refetchInterval: 5000,
  });
  const [text, setText] = useState('');
  const pr = parsePr(text);
  const start = useMutation({
    mutationFn: (number: number) => api<{ id: string }>('/ci', 'POST', { pr: number }),
    onSuccess: ({ id }) => {
      void client.invalidateQueries({ queryKey: ['runs', 'ci'] });
      navigate(`/runs/${id}`);
    },
  });
  const active = runs.data?.find((r) => isActive(r.progress));
  const supersedes = active && pr !== undefined && prNumber(active.record?.facts) === pr;

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
            disabled={pr === undefined || start.isPending || Boolean(ci.unavailable) || (Boolean(active) && !supersedes)}
            title={
              ci.unavailable ??
              (active && !supersedes
                ? `CI of PR #${prNumber(active.record?.facts)} is in progress`
                : supersedes
                  ? 'Cancels the run in progress for this PR, like a new push on GitHub'
                  : undefined)
            }
          >
            <Play className="h-4 w-4" /> {supersedes ? 'Run again' : 'Run CI'}
          </Button>
        </form>
      </section>
      {ci.unavailable && <ErrorNote error={`PR CI is disabled on this host: ${ci.unavailable}`} />}
      <ErrorNote error={start.error} />
      <ErrorNote error={runs.error} />

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
