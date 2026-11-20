import { describe, expect, it } from 'vitest';
import { actions, currentStep, formatDuration, guidance, stepViews } from './runview';
import type { RunDetail } from './types';

const STEPS = ['prepare', 'checks', 'release_pr', 'wait_merge'];

function detail(partial: Partial<RunDetail['progress']>, skipped: string[] = []): RunDetail {
  return {
    progress: {
      id: 'r',
      definition: 'gears-release.v1',
      epoch: 1,
      cursor: 1,
      state: { kind: 'running' },
      created_at: '2026-11-20T10:00:00Z',
      updated_at: '2026-11-20T10:00:00Z',
      steps: [],
      ...partial,
    },
    history: {
      steps: [
        {
          id: 'prepare',
          attempts: [
            {
              number: 1,
              epoch: 1,
              state: {
                kind: 'finished',
                started_at: '2026-11-20T10:00:00Z',
                finished_at: '2026-11-20T10:01:00Z',
                outcome: { kind: 'succeeded' },
              },
            },
          ],
        },
      ],
    },
    record: {
      run_id: 'r',
      started_by: 'a@example.com',
      dry_run: false,
      created_at: '2026-11-20T10:00:00Z',
      facts: { skipped },
    },
  };
}

describe('stepViews', () => {
  it('shows every workflow step, materialized or not', () => {
    const views = stepViews(
      STEPS,
      detail({
        state: { kind: 'retry_wait', due_at: '2026-11-20T11:00:00Z', error: 'pr_open' },
        steps: [
          { id: 'prepare', stage: 0, state: { kind: 'succeeded', completed_at: { kind: 'known', at: '2026-11-20T10:01:00Z' } }, attempts: 1 },
          { id: 'checks', stage: 1, state: { kind: 'succeeded', completed_at: { kind: 'unknown' } }, attempts: 1 },
          { id: 'release_pr', stage: 2, state: { kind: 'succeeded', completed_at: { kind: 'unknown' } }, attempts: 1 },
          { id: 'wait_merge', stage: 3, state: { kind: 'retry_wait', error: 'pr_open' }, attempts: 4 },
        ],
      }),
    );
    expect(views.map((v) => v.status)).toEqual(['succeeded', 'succeeded', 'succeeded', 'retry_wait']);
    expect(views[0].startedAt).toBe('2026-11-20T10:00:00Z');
    expect(views[3]).toMatchObject({ attempts: 4, error: 'pr_open', retryAt: '2026-11-20T11:00:00Z' });
    expect(currentStep(views)?.id).toBe('wait_merge');
  });

  it('marks skipped steps', () => {
    const views = stepViews(
      ['wait_merge'],
      detail(
        { steps: [{ id: 'wait_merge', stage: 3, state: { kind: 'succeeded', completed_at: { kind: 'unknown' } }, attempts: 1 }] },
        ['wait_merge'],
      ),
    );
    expect(views[0].status).toBe('skipped');
  });
});

describe('actions', () => {
  it('offers the commands the engine accepts', () => {
    expect(actions({ kind: 'running' })).toEqual({ cancel: true, resume: false, retry: false });
    expect(actions({ kind: 'cancelled', reason: null, completed_at: { kind: 'unknown' } }).resume).toBe(true);
    expect(actions({ kind: 'failed', error: 'x', completed_at: { kind: 'unknown' } }).retry).toBe(true);
    expect(actions({ kind: 'succeeded', completed_at: { kind: 'unknown' } })).toEqual({ cancel: false, resume: false, retry: false });
  });
});

describe('formatDuration', () => {
  it('scales units', () => {
    const t = '2026-11-20T10:00:00Z';
    expect(formatDuration(t, '2026-11-20T10:00:42Z')).toBe('42s');
    expect(formatDuration(t, '2026-11-20T12:05:00Z')).toBe('2h 5m');
    expect(formatDuration(t, '2026-11-23T13:00:00Z')).toBe('3d 3h');
  });
});

describe('guidance', () => {
  const done = { kind: 'known' as const, at: '2026-11-20T10:01:00Z' };
  const run = (
    state: RunDetail['progress']['state'],
    steps: RunDetail['progress']['steps'],
    facts: NonNullable<RunDetail['record']>['facts'] = {},
    dryRun = false,
  ): RunDetail => {
    const d = detail({ state, steps });
    d.record = { ...d.record!, dry_run: dryRun, facts };
    return d;
  };
  const ok = (id: string, stage: number) => ({ id, stage, state: { kind: 'succeeded' as const, completed_at: done }, attempts: 1 });
  const next = (d: RunDetail) => guidance(stepViews(STEPS, d), d);

  it('asks for the merge while the PR is open', () => {
    const d = run(
      { kind: 'retry_wait', due_at: '2026-11-20T11:00:00Z', error: 'pr_open' },
      [ok('prepare', 0), ok('checks', 1), ok('release_pr', 2), { id: 'wait_merge', stage: 3, state: { kind: 'retry_wait', error: 'pr_open' }, attempts: 2 }],
      { pr: { number: 7, url: 'https://github.com/o/r/pull/7' } },
    );
    expect(next(d)).toMatchObject({ kind: 'action', title: 'Review and merge release PR #7', link: { url: 'https://github.com/o/r/pull/7' } });
  });

  it('says a rate limit needs nobody', () => {
    const d = run({ kind: 'retry_wait', due_at: '2026-11-20T11:00:00Z', error: 'crates_io_429' }, [ok('prepare', 0)]);
    expect(next(d).kind).toBe('wait');
  });

  it('sends failed checks to a new release, not Retry', () => {
    const d = run({ kind: 'failed', error: 'checks_failed', completed_at: done }, [
      ok('prepare', 0),
      { id: 'checks', stage: 1, state: { kind: 'failed', error: 'checks_failed', completed_at: done }, attempts: 1 },
    ]);
    expect(next(d)).toMatchObject({ kind: 'action', button: 'new_release' });
  });

  it('points host problems at the host', () => {
    const d = run({ kind: 'failed', error: 'registry_token_missing', completed_at: done }, [ok('prepare', 0)]);
    expect(next(d)).toMatchObject({ kind: 'admin', button: 'retry' });
  });

  it('offers the real release after a dry run', () => {
    const d = run({ kind: 'succeeded', completed_at: done }, STEPS.map(ok), {}, true);
    expect(next(d)).toMatchObject({ kind: 'action', button: 'new_release' });
  });
});
