import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Play } from 'lucide-react';
import { api } from './api';
import { navigate } from './router';
import { formatDuration, formatTime, guidance, isActive, statusLabel, stepViews, summaryTitle, tone } from './runview';
import type { RunSummary } from './types';
import { Badge, Button, ErrorNote } from './ui';
import { Quip } from './irony';

export function RunsPage({ steps }: { steps: string[] }) {
  const client = useQueryClient();
  const runs = useQuery({
    queryKey: ['runs'],
    queryFn: () => api<RunSummary[]>('/runs'),
    refetchInterval: 5000,
  });
  const [dryRun, setDryRun] = useState(true);
  const [confirming, setConfirming] = useState(false);
  const start = useMutation({
    mutationFn: () => api<{ id: string }>('/runs', 'POST', { dry_run: dryRun }),
    onSuccess: ({ id }) => {
      void client.invalidateQueries({ queryKey: ['runs'] });
      navigate(`/runs/${id}`);
    },
    onSettled: () => setConfirming(false),
  });
  const active = runs.data?.find((r) => isActive(r.progress));

  return (
    <div className="space-y-4">
      <section className="flex flex-wrap items-center gap-3 rounded-xl bg-white p-4 shadow-sm ring-1 ring-slate-200">
        <div className="mr-auto">
          <h1 className="text-base font-semibold">Release gears-rust</h1>
          <p className="text-sm text-slate-600">
            Checks, release PR, wait for merge, publish to crates.io with rate-limit retries, tag healing
            and verification.
          </p>
          <Quip className="mt-0.5">Everything release-plz.yml did on GitHub Actions, back when GitHub let us.</Quip>
        </div>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={dryRun}
            onChange={(e) => {
              setDryRun(e.target.checked);
              setConfirming(false);
            }}
          />
          dry run
        </label>
        {confirming ? (
          <>
            <Button onClick={() => setConfirming(false)}>Back</Button>
            <Button variant="danger" disabled={start.isPending} onClick={() => start.mutate()}>
              <Play className="h-4 w-4" /> Confirm: publish to crates.io
            </Button>
          </>
        ) : (
          <Button
            variant="primary"
            disabled={start.isPending || Boolean(active)}
            title={active ? 'A run is in progress' : undefined}
            onClick={() => (dryRun ? start.mutate() : setConfirming(true))}
          >
            <Play className="h-4 w-4" /> {dryRun ? 'Start dry run' : 'Start release'}
          </Button>
        )}
      </section>
      <ErrorNote error={start.error} />
      <ErrorNote error={runs.error} />

      <section className="overflow-hidden rounded-xl bg-white shadow-sm ring-1 ring-slate-200">
        <table className="w-full text-left text-sm">
          <thead className="bg-slate-50 text-xs uppercase tracking-wide text-slate-500">
            <tr>
              <th className="px-4 py-2 font-medium">Run</th>
              <th className="px-4 py-2 font-medium">Status</th>
              <th className="px-4 py-2 font-medium">Started</th>
              <th className="px-4 py-2 font-medium">Duration</th>
              <th className="px-4 py-2 font-medium">By</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-slate-100">
            {runs.data?.map((run) => (
              <tr
                key={run.progress.id}
                className="cursor-pointer hover:bg-slate-50"
                onClick={() => navigate(`/runs/${run.progress.id}`)}
              >
                <td className="px-4 py-2.5">
                  <div className="font-medium">{summaryTitle(run)}</div>
                  <div className="font-mono text-xs text-slate-400">{run.progress.id.slice(0, 8)}</div>
                </td>
                <td className="px-4 py-2.5">
                  <div className="flex items-center gap-1.5">
                    <Badge tone={tone(run.progress.state)}>{statusLabel(run.progress.state)}</Badge>
                    {run.record?.dry_run && <Badge tone="neutral">dry run</Badge>}
                    <NeedsYou run={run} steps={steps} />
                  </div>
                </td>
                <td className="px-4 py-2.5 text-slate-600">{formatTime(run.progress.created_at)}</td>
                <td className="px-4 py-2.5 text-slate-600">
                  {formatDuration(
                    run.progress.created_at,
                    isActive(run.progress) ? undefined : run.progress.updated_at,
                  )}
                </td>
                <td className="px-4 py-2.5 text-slate-600">{run.record?.started_by ?? '—'}</td>
              </tr>
            ))}
            {runs.data?.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-8 text-center text-slate-500">
                  No runs yet.
                  <Quip className="mt-1">Just like our GitHub Actions tab, except this time it's our choice.</Quip>
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </section>
    </div>
  );
}

/** Marks runs that wait for a person (merge the PR, fix the host…); only active or
 * the newest settled ones matter, so finished history stays quiet. */
function NeedsYou({ run, steps }: { run: RunSummary; steps: string[] }) {
  const next = guidance(stepViews(steps, run), run);
  if (next.kind !== 'action' && next.kind !== 'admin') return null;
  if (!isActive(run.progress) && next.button === 'new_release') return null;
  return (
    <span title={next.title}>
      <Badge tone={next.kind === 'admin' ? 'bad' : 'warn'}>{next.kind === 'admin' ? 'host needs fixing' : 'needs you'}</Badge>
    </span>
  );
}
