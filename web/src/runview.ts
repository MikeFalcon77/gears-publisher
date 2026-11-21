import type { RecordedTime, ReleaseFacts, RunHistory, RunProgress, RunState, RunSummary } from './types';

export type StepStatus =
  | 'pending'
  | 'running'
  | 'retry_wait'
  | 'succeeded'
  | 'skipped'
  | 'failed'
  | 'cancelled';

export interface StepView {
  id: string;
  status: StepStatus;
  attempts: number;
  error?: string;
  startedAt?: string;
  finishedAt?: string;
  /** When the engine retries this step (run-level `retry_wait`). */
  retryAt?: string;
}

export function known(time: RecordedTime | undefined): string | undefined {
  return time?.kind === 'known' ? time.at : undefined;
}

/** Merges progress, attempt history and facts into one row per workflow step. */
export function stepViews(stepIds: string[], detail: RunSummary & { history?: RunHistory }): StepView[] {
  const { progress, history, record } = detail;
  const skipped = new Set(record?.facts.skipped ?? []);
  return stepIds.map((id) => {
    const step = progress.steps.find((s) => s.id === id);
    const attempts = history?.steps.find((s) => s.id === id)?.attempts ?? [];
    const first = attempts[0]?.state;
    const last = attempts[attempts.length - 1]?.state;
    const view: StepView = {
      id,
      status: 'pending',
      attempts: step?.attempts ?? attempts.length,
      startedAt: first?.started_at,
      finishedAt: last?.kind === 'finished' ? last.finished_at : undefined,
    };
    if (!step) return view;
    switch (step.state.kind) {
      case 'running':
        view.status = 'running';
        view.finishedAt = undefined;
        break;
      case 'retry_wait':
        view.status = 'retry_wait';
        view.error = step.state.error;
        if (progress.state.kind === 'retry_wait') view.retryAt = progress.state.due_at ?? undefined;
        break;
      case 'succeeded':
        view.status = skipped.has(id) ? 'skipped' : 'succeeded';
        view.finishedAt = known(step.state.completed_at) ?? view.finishedAt;
        break;
      case 'failed':
        view.status = 'failed';
        view.error = step.state.error;
        break;
      case 'cancelled':
        view.status = 'cancelled';
        break;
      default:
        break;
    }
    return view;
  });
}

export function statusLabel(state: RunState): string {
  switch (state.kind) {
    case 'retry_wait':
      return 'waiting to retry';
    case 'compensation_failed':
      return 'compensation failed';
    default:
      return state.kind;
  }
}

export type Tone = 'neutral' | 'active' | 'good' | 'bad' | 'warn';

export function tone(state: RunState): Tone {
  switch (state.kind) {
    case 'succeeded':
      return 'good';
    case 'failed':
    case 'blocked':
    case 'compensation_failed':
      return 'bad';
    case 'cancelled':
    case 'cancelling':
    case 'retry_wait':
      return 'warn';
    case 'running':
    case 'queued':
    case 'failing':
      return 'active';
    default:
      return 'neutral';
  }
}

export interface Actions {
  cancel: boolean;
  resume: boolean;
  retry: boolean;
}

export function actions(state: RunState): Actions {
  return {
    cancel: ['queued', 'running', 'retry_wait', 'failing'].includes(state.kind),
    resume: state.kind === 'cancelled',
    retry: state.kind === 'failed' || state.kind === 'compensation_failed',
  };
}

export function isActive(progress: RunProgress): boolean {
  return actions(progress.state).cancel || progress.state.kind === 'cancelling';
}

/** The step the run is on: the first one not yet finished. */
export function currentStep(views: StepView[]): StepView | undefined {
  return views.find((v) => v.status !== 'succeeded' && v.status !== 'skipped');
}

export function summaryTitle(run: RunSummary): string {
  const facts = run.record?.facts;
  if (facts?.nothing_to_release) return 'Nothing to release';
  const released = facts?.releases ?? facts?.publish?.releases;
  if (run.progress.state.kind === 'succeeded' && !run.record?.dry_run && released?.length)
    return `Released ${released.length} crate${released.length === 1 ? '' : 's'}`;
  if (facts?.pr) return `Release PR #${facts.pr.number}`;
  return run.record?.dry_run ? 'Dry run' : 'Release';
}

export function formatTime(iso: string | undefined): string {
  if (!iso) return '';
  return new Date(iso).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

export function formatDuration(fromIso?: string, toIso?: string, now = Date.now()): string {
  if (!fromIso) return '';
  const ms = (toIso ? Date.parse(toIso) : now) - Date.parse(fromIso);
  if (!Number.isFinite(ms) || ms < 0) return '';
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** What happens next, and whether someone has to do it. */
export interface Guidance {
  /** `action`: a person must act; `admin`: the host needs fixing; `wait`: it runs by
   * itself; `done`: nothing left to do. */
  kind: 'action' | 'admin' | 'wait' | 'done';
  title: string;
  detail?: string;
  link?: { label: string; url: string };
  /** The button that continues from here. */
  button?: 'retry' | 'resume' | 'new_release';
  /** A joke about why this app exists; only on good or waiting news. */
  quip?: string;
}

const STEP_DOING: Record<string, string> = {
  prepare: 'Fetching main',
  checks: 'Running the CI checks (fmt, clippy, deny, dylint, tests); this takes a while',
  pre_release: 'Finishing any earlier unfinished release',
  release_pr: 'Opening the release PR',
  sync_main: 'Checking out the merge commit',
  publish: 'Publishing to crates.io',
  heal_tags: 'Tagging the released crates',
  verify: 'Checking crates.io and tags',
};

function failureGuidance(step: string, code: string, facts: ReleaseFacts): Guidance {
  switch (code) {
    case 'checks_failed':
      return {
        kind: 'action',
        title: 'A CI check failed',
        detail:
          'See Problems. If main needs a fix, merge it and start a new release: Retry checks the same commit again. ' +
          'Press Retry only for a flaky test or a problem on the host.',
        button: 'new_release',
      };
    case 'pr_closed':
      return {
        kind: 'action',
        title: `Release PR #${facts.pr?.number ?? ''} was closed without merging`,
        detail: 'Nothing was published. Start a new release when main is ready.',
        button: 'new_release',
      };
    case 'registry_token_missing':
      return {
        kind: 'admin',
        title: 'CARGO_REGISTRY_TOKEN is not configured on the host',
        detail: 'Run deploy/deploy.sh secrets <ip> <env-file>, then press Retry.',
        button: 'retry',
      };
    case 'gh_not_logged_in':
      return {
        kind: 'admin',
        title: 'The host is not logged in to GitHub',
        detail: 'Run `sudo -iu publisher gh auth login` on the host, then press Retry.',
        button: 'retry',
      };
    case 'publish_failed':
      return {
        kind: 'action',
        title: 'release-plz release failed repeatedly',
        detail:
          'Not a rate limit: see Problems. Fix the cause, then press Retry. It continues from this step; ' +
          'crates that are already on crates.io are skipped.',
        button: 'retry',
      };
    case 'verify_mismatch':
      return {
        kind: 'action',
        title: 'crates.io or the tags do not match the release',
        detail: 'The Report tab shows which crates differ. Press Retry to check again once crates.io has caught up.',
        button: 'retry',
      };
    default:
      return {
        kind: 'action',
        title: `${step} failed: ${code}`,
        detail: 'See Problems. Press Retry to continue from this step.',
        button: 'retry',
      };
  }
}

/** The next thing that happens in a run, in words a person can act on. */
export function guidance(views: StepView[], detail: RunSummary): Guidance {
  const { progress, record } = detail;
  const facts = record?.facts ?? {};
  const dryRun = record?.dry_run ?? false;
  const current = currentStep(views);
  const state = progress.state;
  switch (state.kind) {
    case 'queued':
      return { kind: 'wait', title: 'Queued; starts in a moment', quip: 'Queued. Unlike on GitHub, it will actually start.' };
    case 'succeeded':
      if (facts.nothing_to_release)
        return {
          kind: 'done',
          title: 'Nothing to release',
          detail: 'Every crate on main is already on crates.io.',
          quip: "Nothing to release: still the most productive Actions run we've had since November 13.",
        };
      if (dryRun)
        return {
          kind: 'action',
          title: 'Dry run passed',
          detail: 'Start a real release when you are ready: it opens the release PR for you to merge.',
          button: 'new_release',
          quip: 'A dry run. Our GitHub Actions tab has been dry for a while too.',
        };
      return {
        kind: 'done',
        title: `Released ${(facts.releases ?? facts.publish?.releases ?? []).length} packages`,
        detail: 'Published to crates.io, tagged and verified.',
        quip: 'Shipped. GitHub Actions could never (literally: it is disabled for us).',
      };
    case 'cancelled':
      return {
        kind: 'action',
        title: 'Cancelled',
        detail: 'Resume continues from where it stopped; or start a new release.',
        button: 'resume',
      };
    case 'cancelling':
      return { kind: 'wait', title: 'Cancelling; stopping the running command' };
    case 'failed':
    case 'compensation_failed': {
      const failed = views.find((v) => v.status === 'failed') ?? current;
      return failureGuidance(failed?.id ?? 'a step', state.error, facts);
    }
    default:
      break;
  }

  if (current?.id === 'wait_merge' && facts.pr)
    return {
      kind: 'action',
      title: `Review and merge release PR #${facts.pr.number}`,
      detail:
        'The release continues by itself after the merge (checked every minute): it publishes the crates and tags them. ' +
        'Closing the PR without merging stops the run.',
      link: { label: `Open PR #${facts.pr.number}`, url: facts.pr.url },
      quip: "Merging PRs: the part of GitHub we're still allowed to use.",
    };
  if (state.kind === 'retry_wait') {
    const when = state.due_at ? ` at ${formatTime(state.due_at)}` : '';
    if (state.error === 'crates_io_429')
      return {
        kind: 'wait',
        title: `crates.io rate limit; next attempt${when}`,
        detail: 'Nothing to do: publishing continues by itself.',
        quip: "crates.io says slow down. At least it didn't flag us.",
      };
    if (state.error === 'gh_not_logged_in' || state.error === 'registry_token_missing')
      return { ...failureGuidance(current?.id ?? '', state.error, facts), button: undefined };
    return {
      kind: 'wait',
      title: `${current?.id ?? 'A step'} failed (${state.error}); retrying${when}`,
      detail: 'It retries by itself. Watch Problems if it keeps failing.',
    };
  }
  return { kind: 'wait', title: current ? (STEP_DOING[current.id] ?? `Running ${current.id}`) : 'Running' };
}

/** What `verify` found wrong with one released crate. */
export type VerifyIssue = 'not_on_crates_io' | 'no_tag' | 'lookup_error';

export interface ReportRow {
  name: string;
  version: string;
  /** The crates.io version it replaced. */
  previous?: string;
  /** Whether `previous` was looked up: false before verify and for runs older than it. */
  previousKnown: boolean;
  /** Released as a tag only (`git_only`), not to crates.io. */
  gitOnly: boolean;
  tag: string;
  crateUrl?: string;
  tagUrl?: string;
  compareUrl?: string;
  /** `false` until verify has run. */
  verified: boolean;
  issues: VerifyIssue[];
}

export interface ReleaseReport {
  rows: ReportRow[];
  /** Differences `verify` found outside the released crates. */
  other: string[];
}

/** The release PR's repository (`https://github.com/o/r`), for tag and compare links. */
function repository(facts: ReleaseFacts): string | undefined {
  return /^(https:\/\/github\.com\/[^/]+\/[^/]+)\/pull\/\d+\/?$/.exec(facts.pr?.url ?? '')?.[1];
}

/** What a release published, one row per crate, with what `verify` found for it. */
export function releaseReport(facts: ReleaseFacts): ReleaseReport {
  const releases = facts.releases ?? facts.publish?.releases ?? [];
  const verify = facts.verify;
  const repo = repository(facts);
  const gitOnly = new Set(verify?.git_only ?? []);
  const accounted = new Set<string>();
  const rows = releases
    .map((r): ReportRow => {
      const missing = `${r.package_name} ${r.version}`;
      const error = verify?.errors.find((e) => e.startsWith(`${missing}:`));
      const issues: VerifyIssue[] = [];
      if (verify?.missing_on_crates_io.includes(missing)) issues.push('not_on_crates_io');
      if (verify?.missing_tags.includes(r.tag)) issues.push('no_tag');
      if (error) issues.push('lookup_error');
      accounted.add(missing).add(r.tag);
      if (error) accounted.add(error);
      const tagOnly = gitOnly.has(r.package_name);
      // Runs verified before `previous` existed have none: unknown, not a first release.
      const previousKnown = verify?.previous !== undefined && !tagOnly && !error;
      const previous = verify?.previous?.[r.package_name];
      // Tags are `name-vX.Y.Z`; the previous release's tag differs only in the version.
      const previousTag =
        previous && r.tag.endsWith(`v${r.version}`) ? `${r.tag.slice(0, -r.version.length)}${previous}` : undefined;
      return {
        name: r.package_name,
        version: r.version,
        previous,
        previousKnown,
        gitOnly: tagOnly,
        tag: r.tag,
        crateUrl: tagOnly
          ? undefined
          : `https://crates.io/crates/${encodeURIComponent(r.package_name)}/${encodeURIComponent(r.version)}`,
        tagUrl: repo && `${repo}/tree/${encodeURIComponent(r.tag)}`,
        compareUrl: repo && previousTag && `${repo}/compare/${encodeURIComponent(previousTag)}...${encodeURIComponent(r.tag)}`,
        verified: verify !== undefined && verify !== null,
        issues,
      };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
  const other = verify
    ? [
        ...verify.missing_on_crates_io.filter((x) => !accounted.has(x)).map((x) => `not on crates.io: ${x}`),
        ...verify.missing_tags.filter((x) => !accounted.has(x)).map((x) => `no tag: ${x}`),
        ...verify.errors.filter((x) => !accounted.has(x)).map((x) => `lookup error: ${x}`),
      ]
    : [];
  return { rows, other };
}
