import { describe, expect, it } from 'vitest';
import { ciGuidance, jobCounts, jobViews, parsePr } from './ciview';
import { LogModel } from './logmodel';
import type { CiFacts, CiInfo, CiRun, LogLine, RunState, StepProgress } from './types';

const CI: CiInfo = {
  steps: ['changes', 'fmt', 'clippy', 'test', 'conclusion'],
  jobs: [
    { id: 'fmt', name: 'cargo fmt (check)', workflow: 'fmt.yml', steps: ['cargo fmt (check)'], note: null },
    { id: 'clippy', name: 'Clippy', workflow: 'ci.yml', steps: ['cargo clippy'], note: null },
    { id: 'test', name: 'Test Suite (ubuntu-latest) / nextest', workflow: 'test-suite.yml', steps: ['cargo test'], note: null },
  ],
  repository: 'o/r',
  unavailable: null,
};

const done = { kind: 'known' as const, at: '2026-11-21T10:05:00Z' };

function succeeded(id: string, stage: number): StepProgress {
  return { id, stage, state: { kind: 'succeeded', completed_at: done }, attempts: 1 };
}

function run(state: RunState, facts: CiFacts, steps: StepProgress[] = []): CiRun {
  return {
    kind: 'ci',
    progress: {
      id: 'r',
      definition: 'gears-pr-ci.v1',
      epoch: 1,
      cursor: 1,
      state,
      created_at: '2026-11-21T10:00:00Z',
      updated_at: '2026-11-21T10:05:00Z',
      steps,
    },
    history: { steps: [] },
    record: { run_id: 'r', started_by: 'a@example.com', dry_run: false, created_at: '2026-11-21T10:00:00Z', facts },
  };
}

const FAILED_CLIPPY: CiFacts = {
  pr_number: 42,
  pr: {
    number: 42,
    title: 'Add a thing',
    url: 'https://github.com/o/r/pull/42',
    author: 'dev',
    head_ref: 'dev/thing',
    head_sha: 'aaaa',
    base_ref: 'main',
    cross_repository: false,
  },
  jobs: [
    { id: 'fmt', conclusion: 'skipped', reason: 'no change matches fmt.yml’s paths' },
    { id: 'clippy', conclusion: 'failure', failed_step: 'cargo clippy' },
    { id: 'test', conclusion: 'success' },
  ],
  skipped: ['fmt'],
};

describe('jobViews', () => {
  it('shows a failed job as failed although its step succeeded', () => {
    const r = run({ kind: 'failed', error: 'checks_failed', completed_at: done }, FAILED_CLIPPY, [
      succeeded('changes', 0),
      succeeded('fmt', 1),
      succeeded('clippy', 2),
      succeeded('test', 3),
      { id: 'conclusion', stage: 4, state: { kind: 'failed', error: 'checks_failed', completed_at: done }, attempts: 1 },
    ]);
    const views = jobViews(CI, r);
    expect(views.map((v) => [v.id, v.name, v.status])).toEqual([
      ['changes', 'Detect changes', 'succeeded'],
      ['fmt', 'cargo fmt (check)', 'skipped'],
      ['clippy', 'Clippy', 'failed'],
      ['test', 'Test Suite (ubuntu-latest) / nextest', 'succeeded'],
      ['conclusion', 'Conclusion', 'failed'],
    ]);
    expect(views[1].reason).toBe('no change matches fmt.yml’s paths');
    expect(views[2].failedStep).toBe('cargo clippy');
    expect(views[2].workflow).toBe('ci.yml');
    expect(views[0].workflow).toBeNull();
  });
});

describe('ciGuidance', () => {
  it('asks to re-run the failed jobs', () => {
    const r = run({ kind: 'failed', error: 'checks_failed', completed_at: done }, FAILED_CLIPPY);
    const g = ciGuidance(CI, r, jobViews(CI, r));
    expect(g).toMatchObject({ kind: 'action', title: 'A job failed: Clippy', button: 'rerun_failed' });
  });

  it('blames the host, not the PR, when the disk ran out', () => {
    const facts: CiFacts = {
      pr_number: 42,
      jobs: [
        {
          id: 'test',
          conclusion: 'failure',
          failed_step: 'cargo test',
          host_problem: 'the build host ran out of disk space (not the code’s fault); re-run it',
          failures: ['cf-a::ui ui'],
        },
      ],
    };
    const r = run({ kind: 'failed', error: 'checks_failed', completed_at: done }, facts);
    const views = jobViews(CI, r);
    expect(ciGuidance(CI, r, views)).toMatchObject({ kind: 'admin', button: 'rerun_failed' });
    expect(ciGuidance(CI, r, views).title).toMatch(/build host broke a job, not the PR/);
    const test = views.find((v) => v.id === 'test');
    expect(test?.hostProblem).toMatch(/disk space/);
    expect(test?.failures).toEqual(['cf-a::ui ui']);
  });

  it('names the failing tests of a code failure', () => {
    const facts: CiFacts = {
      pr_number: 42,
      jobs: [{ id: 'test', conclusion: 'failure', failed_step: 'cargo test', failures: ['cf-a::it one', 'cf-b two'] }],
    };
    const r = run({ kind: 'failed', error: 'checks_failed', completed_at: done }, facts);
    const g = ciGuidance(CI, r, jobViews(CI, r));
    expect(g.kind).toBe('action');
    expect(g.detail).toMatch(/^Failed: cf-a::it one; cf-b two\. /);
  });

  it('asks to resolve conflicts, then Retry', () => {
    const r = run({ kind: 'failed', error: 'merge_conflict', completed_at: done }, { pr_number: 42 });
    expect(ciGuidance(CI, r, jobViews(CI, r))).toMatchObject({ kind: 'action', button: 'retry' });
  });

  it('counts passed and skipped jobs on success', () => {
    const facts: CiFacts = { jobs: [{ id: 'fmt', conclusion: 'skipped' }, { id: 'clippy', conclusion: 'success' }] };
    const r = run({ kind: 'succeeded', completed_at: done }, facts);
    expect(ciGuidance(CI, r, jobViews(CI, r)).title).toBe('All checks passed: 1 job');
    expect(jobCounts(facts)).toEqual({ success: 1, failure: 0, skipped: 1 });
  });

  it('names the running job with its workflow', () => {
    const r = run({ kind: 'running' }, FAILED_CLIPPY, [
      succeeded('changes', 0),
      succeeded('fmt', 1),
      { id: 'clippy', stage: 2, state: { kind: 'running', started_at: done }, attempts: 1 },
    ]);
    const facts = { ...FAILED_CLIPPY, jobs: [], skipped: [] };
    const fresh = { ...r, record: { ...r.record!, facts } };
    expect(ciGuidance(CI, fresh, jobViews(CI, fresh)).title).toBe('Running Clippy (ci.yml)');
  });

  it('explains waiting for the merge commit', () => {
    const r = run({ kind: 'retry_wait', due_at: null, error: 'merge_ref_stale' }, { pr_number: 42 });
    expect(ciGuidance(CI, r, jobViews(CI, r)).title).toMatch(/merge commit/);
  });
});

describe('parsePr', () => {
  it('accepts numbers and PR URLs', () => {
    expect(parsePr('42')).toBe(42);
    expect(parsePr(' #42 ')).toBe(42);
    expect(parsePr('https://github.com/constructorfabric/gears-rust/pull/6080')).toBe(6080);
    expect(parsePr('https://github.com/o/r/pull/7/files')).toBe(7);
    expect(parsePr('')).toBeUndefined();
    expect(parsePr('0')).toBeUndefined();
    expect(parsePr('abc')).toBeUndefined();
  });
});

describe('LogModel on PR CI', () => {
  it('reads who started the run and the merge commit it tests', () => {
    const lines: [string, string, string][] = [
      ['control', 'info', 'PR #42 CI started by a@example.com (re-run of clippy)'],
      ['changes', 'info', 'changes: attempt 1 started'],
      ['changes', 'info', 'merge commit is at 3e76e00000000000000000000000000000000002'],
    ];
    const model = new LogModel();
    lines.forEach(([step, stream, line], i) =>
      model.append({ seq: i + 1, run_id: 'r', step, attempt: step === 'control' ? 0 : 1, stream, line, ts: '2026-11-21T10:00:00Z' } as LogLine),
    );
    expect(model.attempts).toHaveLength(1);
    expect(model.attempts[0].by).toBe('a@example.com');
    expect(model.attempts[0].mainSha).toBe('3e76e00000000000000000000000000000000002');
    expect(model.attempts[0].commitLabel).toBe('merge');
  });
});
