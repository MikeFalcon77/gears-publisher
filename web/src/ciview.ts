import { currentStep, formatTime, stepViews, type StepView } from './runview';
import type { CiFacts, CiInfo, CiRun, CiSummary, JobResult } from './types';

/** One row of the jobs list: a workflow step, named as on GitHub. */
export interface JobView extends StepView {
  name: string;
  /** Workflow file, or `null` for the publisher's own steps. */
  workflow: string | null;
  /** The GitHub step that failed. */
  failedStep?: string;
  /** Why it was skipped. */
  reason?: string;
  /** The host, not the code, broke it. */
  hostProblem?: string;
  /** Failing tests and crates that did not compile. */
  failures?: string[];
}

const OWN_STEPS: Record<string, string> = {
  changes: 'Detect changes',
  conclusion: 'Conclusion',
};

/**
 * Merges progress, history and the jobs' results. A job whose check failed is a
 * step that succeeded (the run goes on to the other jobs), so the result decides.
 */
export function jobViews(ci: CiInfo, run: CiSummary & { history?: CiRun['history'] }): JobView[] {
  const results = new Map((run.record?.facts.jobs ?? []).map((j) => [j.id, j]));
  return stepViews(ci.steps, run).map((view) => {
    const job = ci.jobs.find((j) => j.id === view.id);
    const result = results.get(view.id);
    const row: JobView = {
      ...view,
      name: job?.name ?? OWN_STEPS[view.id] ?? view.id,
      workflow: job?.workflow ?? null,
    };
    if (result?.conclusion === 'failure') {
      row.status = 'failed';
      row.failedStep = result.failed_step ?? undefined;
      row.hostProblem = result.host_problem ?? undefined;
      row.failures = result.failures?.length ? result.failures : undefined;
      row.error = undefined;
    } else if (result?.conclusion === 'skipped') {
      row.status = 'skipped';
      row.reason = result.reason ?? undefined;
    }
    return row;
  });
}

export function failedJobs(facts: CiFacts | undefined): JobResult[] {
  return (facts?.jobs ?? []).filter((j) => j.conclusion === 'failure');
}

/** `Failed: a; b. ` for the failing tests the jobs named, or nothing. */
function failedNames(failed: JobResult[]): string {
  const names = failed.flatMap((j) => j.failures ?? []);
  return names.length ? `Failed: ${names.join('; ')}. ` : '';
}

export interface JobCounts {
  success: number;
  failure: number;
  skipped: number;
}

export function jobCounts(facts: CiFacts | undefined): JobCounts {
  const counts = { success: 0, failure: 0, skipped: 0 };
  for (const job of facts?.jobs ?? []) counts[job.conclusion] += 1;
  return counts;
}

export function prNumber(facts: CiFacts | undefined): number | undefined {
  return facts?.pr?.number ?? facts?.pr_number;
}

/** What happens next in a CI run, and the button that continues it. */
export interface CiGuidance {
  kind: 'action' | 'admin' | 'wait' | 'done';
  title: string;
  detail?: string;
  /** `rerun_failed` and `rerun` start a new run; `retry` continues this one. */
  button?: 'rerun_failed' | 'rerun' | 'retry';
}

export function ciGuidance(ci: CiInfo, run: CiRun, views: JobView[]): CiGuidance {
  const state = run.progress.state;
  const facts = run.record?.facts;
  const name = (id: string) => ci.jobs.find((j) => j.id === id)?.name ?? id;
  switch (state.kind) {
    case 'queued':
      return { kind: 'wait', title: 'Queued; starts in a moment' };
    case 'succeeded': {
      const counts = jobCounts(facts);
      if (counts.success === 0)
        return {
          kind: 'done',
          title: 'No job had anything to check',
          detail: 'Every job was skipped by the workflows’ path filters, as GitHub would skip them.',
        };
      return {
        kind: 'done',
        title: `All checks passed: ${counts.success} job${counts.success === 1 ? '' : 's'}`,
        detail: counts.skipped ? `${counts.skipped} skipped by the path filters.` : undefined,
      };
    }
    case 'cancelled':
      return { kind: 'action', title: 'Cancelled', detail: 'Re-run starts it again on the PR’s current head.', button: 'rerun' };
    case 'cancelling':
      return { kind: 'wait', title: 'Cancelling; stopping the running command' };
    case 'failed':
    case 'compensation_failed': {
      const failed = failedJobs(facts);
      if (state.error === 'checks_failed' && failed.length && failed.every((j) => j.host_problem))
        return {
          kind: 'admin',
          title: `The build host broke ${failed.length === 1 ? 'a job' : `${failed.length} jobs`}, not the PR: ${failed.map((j) => name(j.id)).join(', ')}`,
          detail: `${failed[0].host_problem}.`,
          button: 'rerun_failed',
        };
      if (state.error === 'checks_failed' && failed.length)
        return {
          kind: 'action',
          title: `${failed.length === 1 ? 'A job' : `${failed.length} jobs`} failed: ${failed.map((j) => name(j.id)).join(', ')}`,
          detail:
            failedNames(failed) +
            'See Problems. Push a fix to the PR, then Re-run failed jobs: it tests the PR’s current head. ' +
            'For a flaky test, re-run with no new commits.',
          button: 'rerun_failed',
        };
      if (state.error === 'merge_conflict')
        return {
          kind: 'action',
          title: `PR #${prNumber(facts)} conflicts with ${facts?.pr?.base_ref ?? 'its base'}`,
          detail: 'GitHub has no merge commit to test. Resolve the conflicts, then press Retry.',
          button: 'retry',
        };
      if (state.error === 'pr_not_open')
        return { kind: 'done', title: `PR #${prNumber(facts)} is closed`, detail: 'CI runs on open pull requests.' };
      if (state.error === 'gh_not_logged_in')
        return {
          kind: 'admin',
          title: 'The host is not logged in to GitHub',
          detail: 'Run `sudo -iu publisher gh auth login` on the host, then press Retry.',
          button: 'retry',
        };
      const step = views.find((v) => v.status === 'failed') ?? currentStep(views);
      return {
        kind: 'action',
        title: `${step?.name ?? 'A step'} failed: ${state.error}`,
        detail: 'See Problems. Retry continues from this step; Re-run starts over on the PR’s current head.',
        button: 'retry',
      };
    }
    default:
      break;
  }
  const current = currentStep(views);
  if (state.kind === 'retry_wait') {
    const when = state.due_at ? ` at ${formatTime(state.due_at)}` : '';
    if (state.error === 'merge_ref_pending' || state.error === 'merge_ref_stale')
      return {
        kind: 'wait',
        title: `Waiting for GitHub to compute the merge commit; next check${when}`,
        detail: 'GitHub updates refs/pull/N/merge in the background after a push.',
      };
    return {
      kind: 'wait',
      title: `${current?.name ?? 'A step'} failed (${state.error}); retrying${when}`,
      detail: 'It retries by itself. Watch Problems if it keeps failing.',
    };
  }
  if (!current) return { kind: 'wait', title: 'Running' };
  if (current.id === 'changes') return { kind: 'wait', title: 'Fetching the PR and detecting changes' };
  if (current.id === 'conclusion') return { kind: 'wait', title: 'Summing up' };
  return { kind: 'wait', title: `Running ${current.name}${current.workflow ? ` (${current.workflow})` : ''}` };
}

/** A PR number out of `123`, `#123` or a pull request URL. */
export function parsePr(text: string): number | undefined {
  const t = text.trim();
  const match = /^#?(\d+)$/.exec(t) ?? /\/pull\/(\d+)(?:[/?#].*)?$/.exec(t);
  const n = match ? Number(match[1]) : NaN;
  return Number.isSafeInteger(n) && n > 0 ? n : undefined;
}

/** Where a queued PR CI stands, by its place in the queue (1: next). */
export function queuePlace(position: number): string {
  if (position <= 1) return 'Next: starts when the run in progress finishes';
  const ahead = position - 1;
  return `${ahead} ${ahead === 1 ? 'request' : 'requests'} ahead`;
}
