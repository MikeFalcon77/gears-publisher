import { useEffect, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ArrowLeft, CheckCircle2, ExternalLink, Hand, Hourglass, Pause, Play, RotateCcw, Wrench } from 'lucide-react';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { api } from './api';
import { navigate } from './router';
import {
  actions,
  currentStep,
  formatDuration,
  formatTime,
  guidance,
  releaseReport,
  statusLabel,
  type Guidance,
  stepViews,
  tone,
  type StepView,
} from './runview';
import type { CiRun, Me, ReleaseFacts, RunDetail } from './types';
import { CiRunView } from './CiRunPage';
import { Badge, Button, Details, ErrorNote, StepIcon } from './ui';
import { LogViewer } from './LogViewer';
import { ReleaseReport } from './ReleaseReport';
import { useLog } from './uselog';
import { Quip } from './irony';

export function RunPage({ id, me }: { id: string; me: Me }) {
  const client = useQueryClient();
  const detail = useQuery({
    queryKey: ['run', id],
    queryFn: () => api<RunDetail>(`/runs/${id}`),
    refetchInterval: 10000,
  });
  const log = useLog(id, () => void client.invalidateQueries({ queryKey: ['run', id] }));
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  const command = useMutation({
    mutationFn: (name: 'cancel' | 'resume' | 'retry') => api(`/runs/${id}/${name}`, 'POST'),
    onSuccess: () => log.wake(),
    onSettled: () => void client.invalidateQueries({ queryKey: ['run', id] }),
  });

  if (detail.isPending) return <p className="text-slate-500">Loading…</p>;
  if (detail.error) return <ErrorNote error={detail.error} />;
  const run = detail.data;
  if (run.kind === 'ci')
    return (
      <CiRunView
        id={id}
        run={run as unknown as CiRun}
        ci={me.ci}
        log={log}
        now={now}
        busy={command.isPending}
        error={command.error}
        onCommand={(name) => command.mutate(name)}
      />
    );
  const steps = me.steps;
  const views = stepViews(steps, run);
  const current = currentStep(views);
  const allowed = actions(run.progress.state);
  const facts = run.record?.facts ?? {};
  const next = guidance(views, run);
  // A real release that published something gets a report; a dry run keeps its facts.
  const reported = !run.record?.dry_run && releaseReport(facts).rows.length > 0;
  const report = reported ? <ReleaseReport run={run} /> : undefined;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 lg:flex-row">
      <aside className="flex w-full shrink-0 flex-col gap-4 lg:w-[380px]">
        <NextStep
          next={next}
          busy={command.isPending}
          onRetry={allowed.retry ? () => command.mutate('retry') : undefined}
          onResume={allowed.resume ? () => command.mutate('resume') : undefined}
        />
        <section className="space-y-3 rounded-xl bg-white p-4 shadow-sm ring-1 ring-slate-200">
          <button
            className="flex items-center gap-1 text-sm text-slate-500 hover:text-slate-800"
            onClick={() => navigate('/')}
          >
            <ArrowLeft className="h-4 w-4" /> All runs
          </button>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="font-mono text-sm font-semibold">{id.slice(0, 8)}</h1>
            <Badge tone={tone(run.progress.state)}>{statusLabel(run.progress.state)}</Badge>
            {run.record?.dry_run && <Badge tone="neutral">dry run</Badge>}
          </div>
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-sm">
            <dt className="text-slate-500">Started</dt>
            <dd>
              {formatTime(run.progress.created_at)}
              {run.record && <span className="text-slate-500"> by {run.record.started_by}</span>}
            </dd>
            <dt className="text-slate-500">Main</dt>
            <dd className="font-mono text-xs leading-5">{facts.main_sha?.slice(0, 12) ?? '—'}</dd>
            {'error' in run.progress.state && (
              <>
                <dt className="text-slate-500">Error</dt>
                <dd className="font-mono text-xs leading-5 text-rose-700">{run.progress.state.error}</dd>
              </>
            )}
          </dl>
          <div className="flex flex-wrap gap-2">
            {allowed.cancel && (
              <Button disabled={command.isPending} onClick={() => command.mutate('cancel')}>
                <Pause className="h-4 w-4" /> Cancel
              </Button>
            )}
            {allowed.resume && (
              <Button variant="primary" disabled={command.isPending} onClick={() => command.mutate('resume')}>
                <Play className="h-4 w-4" /> Resume
              </Button>
            )}
            {allowed.retry && (
              <Button variant="primary" disabled={command.isPending} onClick={() => command.mutate('retry')}>
                <RotateCcw className="h-4 w-4" /> Retry {current?.id}
              </Button>
            )}
          </div>
          <ErrorNote error={command.error} />
        </section>

        <section className="rounded-xl bg-white p-2 shadow-sm ring-1 ring-slate-200">
          <ol>
            {views.map((view) => (
              <StepRow
                key={view.id}
                view={view}
                now={now}
                facts={facts}
                selected={log.filter === view.id}
                onSelect={() => log.setFilter(log.filter === view.id ? 'all' : view.id)}
              />
            ))}
          </ol>
        </section>

        <Facts facts={facts} reported={reported} />
      </aside>

      <LogViewer runId={id} log={log} steps={steps} state={run.progress.state} report={report} />
    </div>
  );
}

const nextStyle: Record<Guidance['kind'], { icon: typeof Hand; className: string; label: string }> = {
  action: { icon: Hand, className: 'border-amber-300 bg-amber-50 text-amber-950', label: 'Your turn' },
  admin: { icon: Wrench, className: 'border-rose-300 bg-rose-50 text-rose-950', label: 'Host needs fixing' },
  wait: { icon: Hourglass, className: 'border-sky-200 bg-sky-50 text-sky-950', label: 'In progress' },
  done: { icon: CheckCircle2, className: 'border-emerald-200 bg-emerald-50 text-emerald-950', label: 'Done' },
};

/** What happens next and who has to do it. */
function NextStep({
  next,
  busy,
  onRetry,
  onResume,
}: {
  next: Guidance;
  busy: boolean;
  onRetry?: () => void;
  onResume?: () => void;
}) {
  const style = nextStyle[next.kind];
  const Icon = style.icon;
  return (
    <Alert className={style.className}>
      <Icon />
      <AlertTitle className="flex flex-col gap-0.5">
        <span className="text-[11px] font-semibold tracking-wide uppercase opacity-70">{style.label}</span>
        <span className="text-sm leading-snug">{next.title}</span>
      </AlertTitle>
      <AlertDescription className="text-current/80">
        {next.detail && <p>{next.detail}</p>}
        {next.quip && <Quip className="mt-1">{next.quip}</Quip>}
        <div className="mt-2 flex flex-wrap gap-2">
          {next.link && (
            <a
              href={next.link.url}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 rounded-md bg-slate-900 px-3 py-1.5 text-xs font-medium text-white hover:bg-slate-700"
            >
              {next.link.label} <ExternalLink className="h-3.5 w-3.5" />
            </a>
          )}
          {next.button === 'retry' && onRetry && (
            <Button variant="primary" disabled={busy} onClick={onRetry}>
              <RotateCcw className="h-4 w-4" /> Retry
            </Button>
          )}
          {next.button === 'resume' && onResume && (
            <Button variant="primary" disabled={busy} onClick={onResume}>
              <Play className="h-4 w-4" /> Resume
            </Button>
          )}
          {next.button === 'new_release' && (
            <Button variant={next.kind === 'action' ? 'primary' : undefined} onClick={() => navigate('/')}>
              Start a new release
            </Button>
          )}
        </div>
      </AlertDescription>
    </Alert>
  );
}

function StepRow({
  view,
  now,
  facts,
  selected,
  onSelect,
}: {
  view: StepView;
  now: number;
  facts: ReleaseFacts;
  selected: boolean;
  onSelect: () => void;
}) {
  const running = view.status === 'running' || view.status === 'retry_wait';
  const waitingForMerge = view.id === 'wait_merge' && running && facts.pr;
  return (
    <li>
      <button
        onClick={onSelect}
        className={`flex w-full items-start gap-3 rounded-lg px-2 py-2 text-left hover:bg-slate-50 ${selected ? 'bg-sky-50 hover:bg-sky-50' : ''}`}
        title="Show this step's log"
      >
        <StepIcon status={view.status} />
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2">
            <span className="font-mono text-sm">{view.id}</span>
            {view.attempts > 1 && <span className="text-xs text-slate-500">attempt {view.attempts}</span>}
            <span className="ml-auto text-xs text-slate-500">
              {view.status === 'skipped'
                ? 'skipped'
                : formatDuration(view.startedAt, running ? undefined : view.finishedAt, now)}
            </span>
          </div>
          {waitingForMerge && (
            <a
              href={facts.pr!.url}
              target="_blank"
              rel="noreferrer"
              onClick={(e) => e.stopPropagation()}
              className="mt-0.5 inline-flex items-center gap-1 text-xs text-sky-700 hover:underline"
            >
              waiting for merge of PR #{facts.pr!.number} <ExternalLink className="h-3 w-3" />
            </a>
          )}
          {view.error && !waitingForMerge && (
            <div className="mt-0.5 font-mono text-xs text-rose-700">{view.error}</div>
          )}
          {view.retryAt && (
            <div className="mt-0.5 text-xs text-amber-700">
              next attempt {formatTime(view.retryAt)} (in {formatDuration(new Date(now).toISOString(), view.retryAt, now) || 'a moment'})
            </div>
          )}
        </div>
      </button>
    </li>
  );
}

/** `reported`: the released crates and their verification are on the Report tab. */
function Facts({ facts, reported }: { facts: ReleaseFacts; reported: boolean }) {
  const releases = reported ? undefined : (facts.releases ?? facts.publish?.releases);
  const verify = reported ? undefined : facts.verify;
  if (!facts.pr && !facts.pre_released?.length && !facts.packages?.length && !releases && !verify && !facts.update_diff && !facts.nothing_to_release)
    return null;
  return (
    <section className="space-y-3 rounded-xl bg-white p-4 text-sm shadow-sm ring-1 ring-slate-200">
      {facts.nothing_to_release && <p className="text-slate-600">Nothing to release.</p>}
      {!!facts.pre_released?.length && (
        <Details title={`Catch-up release on main: ${facts.pre_released.length} packages`}>
          {facts.pre_released.map((r) => r.tag).join('\n')}
        </Details>
      )}
      {facts.pr && (
        <a href={facts.pr.url} target="_blank" rel="noreferrer" className="flex items-center gap-1 text-sky-700 hover:underline">
          Release PR #{facts.pr.number} <ExternalLink className="h-3.5 w-3.5" />
        </a>
      )}
      {facts.merge_sha && (
        <p>
          Merged as <span className="font-mono text-xs">{facts.merge_sha.slice(0, 12)}</span>
        </p>
      )}
      {!!facts.packages?.length && (
        <Details title={`${facts.packages.length} packages in the PR`}>
          {facts.packages.map((p) => `${p.package_name} ${p.version}`).join('\n')}
        </Details>
      )}
      {facts.update_diff && <Details title="release-plz update">{facts.update_diff}</Details>}
      {facts.publish?.note && <p className="text-slate-600">{facts.publish.note}</p>}
      {releases && (
        <Details title={`${releases.length} released${facts.publish ? `, attempt ${facts.publish.attempt}` : ''}`}>
          {releases.map((r) => r.tag).join('\n') || '(none)'}
        </Details>
      )}
      {verify && (
        <div>
          <p className={verify.missing_on_crates_io.length + verify.missing_tags.length + verify.errors.length ? 'text-rose-700' : 'text-emerald-700'}>
            Verified {verify.checked} packages: {verify.missing_on_crates_io.length} not on crates.io,{' '}
            {verify.missing_tags.length} without tag, {verify.errors.length} errors
          </p>
          {[...verify.missing_on_crates_io, ...verify.missing_tags, ...verify.errors].length > 0 && (
            <Details title="Differences">
              {[
                ...verify.missing_on_crates_io.map((x) => `crates.io: ${x}`),
                ...verify.missing_tags.map((x) => `tag: ${x}`),
                ...verify.errors.map((x) => `error: ${x}`),
              ].join('\n')}
            </Details>
          )}
        </div>
      )}
    </section>
  );
}

