import { useMutation, useQueryClient } from '@tanstack/react-query';
import { ArrowLeft, CheckCircle2, ExternalLink, Hand, Hourglass, Pause, Play, RotateCcw, Wrench } from 'lucide-react';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { api } from './api';
import { navigate } from './router';
import { ciGuidance, failedJobs, jobViews, prNumber, type CiGuidance, type JobView } from './ciview';
import { actions, formatDuration, formatTime, statusLabel, tone } from './runview';
import type { CiFacts, CiInfo, CiRun } from './types';
import { Badge, Button, Details, ErrorNote, StepIcon } from './ui';
import { LogViewer } from './LogViewer';
import type { Log } from './uselog';

type Command = 'cancel' | 'resume' | 'retry';

const style: Record<CiGuidance['kind'], { icon: typeof Hand; className: string; label: string }> = {
  action: { icon: Hand, className: 'border-amber-300 bg-amber-50 text-amber-950', label: 'Your turn' },
  admin: { icon: Wrench, className: 'border-rose-300 bg-rose-50 text-rose-950', label: 'Host needs fixing' },
  wait: { icon: Hourglass, className: 'border-sky-200 bg-sky-50 text-sky-950', label: 'In progress' },
  done: { icon: CheckCircle2, className: 'border-emerald-200 bg-emerald-50 text-emerald-950', label: 'Done' },
};

/** A PR CI run: what to do next, the PR, its jobs and the log. */
export function CiRunView({
  id,
  run,
  ci,
  log,
  now,
  busy,
  error,
  onCommand,
}: {
  id: string;
  run: CiRun;
  ci: CiInfo;
  log: Log;
  now: number;
  busy: boolean;
  error: unknown;
  onCommand: (name: Command) => void;
}) {
  const client = useQueryClient();
  const facts: CiFacts = run.record?.facts ?? {};
  const number = prNumber(facts);
  const views = jobViews(ci, run);
  const next = ciGuidance(ci, run, views);
  const allowed = actions(run.progress.state);
  const failed = failedJobs(facts).map((j) => j.id);
  const settled = !allowed.cancel && run.progress.state.kind !== 'cancelling';
  // Retry continues this run from the step the engine failed (not a job whose check
  // failed: that step succeeded). After `conclusion` there is nothing left to retry.
  const failedStep = run.progress.steps.find((s) => s.state.kind === 'failed');
  const canRetry = allowed.retry && failedStep !== undefined && failedStep.id !== 'conclusion';

  const rerun = useMutation({
    mutationFn: (jobs?: string[]) => api<{ id: string }>('/ci', 'POST', { pr: number, jobs }),
    onSuccess: ({ id: started }) => {
      void client.invalidateQueries({ queryKey: ['runs', 'ci'] });
      navigate(`/runs/${started}`);
    },
  });
  const busyAny = busy || rerun.isPending;
  const canRerun = number !== undefined && settled && !ci.unavailable;

  const Icon = style[next.kind].icon;
  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 lg:flex-row">
      <aside className="flex w-full shrink-0 flex-col gap-4 lg:w-[400px]">
        <Alert className={style[next.kind].className}>
          <Icon />
          <AlertTitle className="flex flex-col gap-0.5">
            <span className="text-[11px] font-semibold tracking-wide uppercase opacity-70">{style[next.kind].label}</span>
            <span className="text-sm leading-snug">{next.title}</span>
          </AlertTitle>
          <AlertDescription className="text-current/80">
            {next.detail && <p>{next.detail}</p>}
            <div className="mt-2 flex flex-wrap gap-2">
              {facts.pr && (
                <a
                  href={facts.pr.url}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-1 rounded-md bg-slate-900 px-3 py-1.5 text-xs font-medium text-white hover:bg-slate-700"
                >
                  Open PR #{facts.pr.number} <ExternalLink className="h-3.5 w-3.5" />
                </a>
              )}
              {next.button === 'rerun_failed' && canRerun && failed.length > 0 && (
                <Button variant="primary" disabled={busyAny} onClick={() => rerun.mutate(failed)}>
                  <RotateCcw className="h-4 w-4" /> Re-run failed jobs
                </Button>
              )}
              {next.button === 'rerun' && canRerun && (
                <Button variant="primary" disabled={busyAny} onClick={() => rerun.mutate(undefined)}>
                  <RotateCcw className="h-4 w-4" /> Re-run
                </Button>
              )}
              {next.button === 'retry' && canRetry && (
                <Button variant="primary" disabled={busyAny} onClick={() => onCommand('retry')}>
                  <RotateCcw className="h-4 w-4" /> Retry
                </Button>
              )}
            </div>
          </AlertDescription>
        </Alert>

        <section className="space-y-3 rounded-xl bg-white p-4 shadow-sm ring-1 ring-slate-200">
          <button className="flex items-center gap-1 text-sm text-slate-500 hover:text-slate-800" onClick={() => navigate('/ci')}>
            <ArrowLeft className="h-4 w-4" /> PR CI
          </button>
          <div>
            <h1 className="text-sm leading-snug font-semibold">
              {number !== undefined ? `#${number} ` : ''}
              {facts.pr?.title ?? 'Pull request'}
            </h1>
            <div className="mt-1 flex flex-wrap items-center gap-2">
              <Badge tone={tone(run.progress.state)}>{statusLabel(run.progress.state)}</Badge>
              {facts.only?.length ? <Badge tone="neutral">re-run: {facts.only.length} job(s)</Badge> : null}
              {facts.pr?.cross_repository && <Badge tone="warn">fork</Badge>}
              <span className="font-mono text-xs text-slate-400">{id.slice(0, 8)}</span>
            </div>
          </div>
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-sm">
            <dt className="text-slate-500">Started</dt>
            <dd>
              {formatTime(run.progress.created_at)}
              {run.record && <span className="text-slate-500"> by {run.record.started_by}</span>}
            </dd>
            {facts.pr && (
              <>
                <dt className="text-slate-500">Branch</dt>
                <dd className="truncate font-mono text-xs leading-5" title={`${facts.pr.author}: ${facts.pr.head_ref}`}>
                  {facts.pr.head_ref} → {facts.pr.base_ref}
                </dd>
                <dt className="text-slate-500">Head</dt>
                <dd className="font-mono text-xs leading-5">{facts.pr.head_sha.slice(0, 12)}</dd>
              </>
            )}
            <dt className="text-slate-500">Tested</dt>
            <dd className="font-mono text-xs leading-5" title="refs/pull/N/merge: the PR merged into its base">
              {facts.merge_sha ? `${facts.merge_sha.slice(0, 12)} (merge)` : '—'}
            </dd>
            {'error' in run.progress.state && (
              <>
                <dt className="text-slate-500">Error</dt>
                <dd className="font-mono text-xs leading-5 text-rose-700">{run.progress.state.error}</dd>
              </>
            )}
          </dl>
          <div className="flex flex-wrap gap-2">
            {allowed.cancel && (
              <Button disabled={busyAny} onClick={() => onCommand('cancel')}>
                <Pause className="h-4 w-4" /> Cancel
              </Button>
            )}
            {allowed.resume && (
              <Button disabled={busyAny} onClick={() => onCommand('resume')} title="Continue from the step it stopped at">
                <Play className="h-4 w-4" /> Resume
              </Button>
            )}
            {canRetry && (
              <Button disabled={busyAny} onClick={() => onCommand('retry')}>
                <RotateCcw className="h-4 w-4" /> Retry {failedStep?.id}
              </Button>
            )}
            {canRerun && failed.length > 0 && (
              <Button disabled={busyAny} onClick={() => rerun.mutate(failed)} title="A new run of only the failed jobs, on the PR's current head">
                <RotateCcw className="h-4 w-4" /> Re-run failed jobs
              </Button>
            )}
            {canRerun && (
              <Button disabled={busyAny} onClick={() => rerun.mutate(undefined)} title="A new run of every job, on the PR's current head">
                <RotateCcw className="h-4 w-4" /> Re-run all jobs
              </Button>
            )}
          </div>
          <ErrorNote error={error} />
          <ErrorNote error={rerun.error} />
        </section>

        <section className="rounded-xl bg-white p-2 shadow-sm ring-1 ring-slate-200">
          <h2 className="px-2 pt-1 pb-1.5 text-xs font-semibold tracking-wide text-slate-500 uppercase">Jobs</h2>
          <ol>
            {views
              .filter((v) => v.id !== 'conclusion' || v.status !== 'pending')
              .map((view) => (
                <JobRow
                  key={view.id}
                  view={view}
                  now={now}
                  selected={log.filter === view.id}
                  onSelect={() => log.setFilter(log.filter === view.id ? 'all' : view.id)}
                />
              ))}
          </ol>
        </section>

        <Changes facts={facts} />
      </aside>

      <LogViewer runId={id} log={log} steps={ci.steps} state={run.progress.state} />
    </div>
  );
}

function JobRow({ view, now, selected, onSelect }: { view: JobView; now: number; selected: boolean; onSelect: () => void }) {
  const running = view.status === 'running' || view.status === 'retry_wait';
  return (
    <li>
      <button
        onClick={onSelect}
        className={`flex w-full items-start gap-3 rounded-lg px-2 py-2 text-left hover:bg-slate-50 ${selected ? 'bg-sky-50 hover:bg-sky-50' : ''}`}
        title="Show this job's log"
      >
        <StepIcon status={view.status} />
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2">
            <span className={`truncate text-sm ${view.status === 'skipped' ? 'text-slate-500' : 'font-medium'}`}>{view.name}</span>
            {view.attempts > 1 && <span className="text-xs text-slate-500">attempt {view.attempts}</span>}
            <span className="ml-auto shrink-0 text-xs text-slate-500">
              {view.status === 'skipped' ? 'skipped' : formatDuration(view.startedAt, running ? undefined : view.finishedAt, now)}
            </span>
          </div>
          {view.workflow && <div className="font-mono text-[11px] text-slate-400">{view.workflow}</div>}
          {view.failedStep && <div className="mt-0.5 text-xs text-rose-700">failed at: {view.failedStep}</div>}
          {view.reason && <div className="mt-0.5 text-xs text-slate-500">{view.reason}</div>}
          {view.error && <div className="mt-0.5 font-mono text-xs text-rose-700">{view.error}</div>}
          {view.retryAt && <div className="mt-0.5 text-xs text-amber-700">next attempt {formatTime(view.retryAt)}</div>}
        </div>
      </button>
    </li>
  );
}

/** What changed and what the path filters made of it. */
function Changes({ facts }: { facts: CiFacts }) {
  const changes = facts.changes;
  if (!changes) return null;
  const more = (facts.changed_files ?? 0) - (facts.files?.length ?? 0);
  return (
    <section className="space-y-2 rounded-xl bg-white p-4 text-sm shadow-sm ring-1 ring-slate-200">
      <div className="flex flex-wrap gap-1.5">
        {Object.entries(changes.filters).map(([name, hit]) => (
          <Badge key={name} tone={hit ? 'active' : 'neutral'}>
            {hit ? '✓' : '–'} {name}
          </Badge>
        ))}
      </div>
      <p className="text-xs text-slate-500">Path filters from {facts.rules ?? 'the workflows'}.</p>
      <Details title={`${facts.changed_files ?? 0} changed files`}>
        {(facts.files ?? []).join('\n') + (more > 0 ? `\n… and ${more} more` : '')}
      </Details>
    </section>
  );
}
