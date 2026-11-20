import { describe, expect, it } from 'vitest';
import { buildSections, extractProblems, flatten } from './logmodel';
import type { LogLine } from './types';

let seq = 0;
function lines(step: string, attempt: number, spec: [LogLine['stream'], string][]): LogLine[] {
  return spec.map(([stream, line]) => ({
    seq: ++seq,
    run_id: 'r',
    step,
    attempt,
    stream,
    line,
    ts: new Date(Date.UTC(2026, 10, 20, 10, 0, seq)).toISOString(),
  }));
}

const checks = lines('checks', 1, [
  ['info', 'checks: attempt 1 started'],
  ['info', 'make clippy'],
  ['cmd', '$ make clippy'],
  ['stderr', '    Checking cf-gears-foo v0.1.0'],
  ['stderr', 'error: this function has too many lines (108/100)'],
  ['stderr', '   --> libs/foo/src/lib.rs:679:1'],
  ['stderr', '    |'],
  ['stderr', ''],
  ['stderr', 'error[E0308]: mismatched types'],
  ['stderr', '  --> libs/foo/src/x.rs:10:5'],
  ['stderr', ''],
  ['stderr', 'error: could not compile `cf-gears-foo` (lib) due to 2 previous errors'],
  ['stderr', 'make: *** [clippy] Error 101'],
  ['info', 'exit 2'],
  ['info', 'make clippy failed; fix main, then press Retry'],
  ['info', 'checks: failed (checks_failed)'],
]);

const tests = lines('checks', 2, [
  ['cmd', '$ make test-no-macros'],
  ['stdout', '        PASS [   0.004s] cf-gears-foo tests::ok'],
  ['stdout', '        FAIL [   0.010s] cf-gears-foo::lib tests::broken'],
  ['stdout', '──── STDERR:             cf-gears-foo::lib tests::broken'],
  ["stdout", "thread 'tests::broken' panicked at libs/foo/src/lib.rs:42:9:"],
  ['stdout', 'assertion `left == right` failed'],
  ['stdout', '        PASS [   0.002s] cf-gears-foo tests::other'],
  ['stdout', '     Summary [   1.000s] 3 tests run: 2 passed, 1 failed, 0 skipped'],
  ['stdout', '        FAIL [   0.010s] cf-gears-foo::lib tests::broken'],
  ['stderr', 'error: test run failed'],
  ['info', 'exit 2'],
]);

const publish = lines('publish', 1, [
  ['cmd', '$ release-plz release -o json'],
  ['stderr', '2026-11-18T15:16:00Z ERROR failed to release package'],
  ['stderr', ''],
  ['stderr', 'Caused by:'],
  ['stderr', '    the remote server responded with an error (status 429 Too Many Requests): try again after Wed, 18 Nov 2026 15:16:16 GMT'],
  ['info', 'exit 1'],
  ['info', 'publish: failed (crates_io_429), will retry in at least 115s'],
]);

const ok = lines('prepare', 1, [
  ['cmd', '$ git fetch origin --tags --force --prune'],
  ['stderr', 'From github.com:x/y'],
  ['info', 'exit 0'],
  ['info', 'prepare: done'],
]);

describe('buildSections', () => {
  const sections = buildSections([...ok, ...checks, ...tests, ...publish]);

  it('groups output by step attempt and command', () => {
    expect(sections.map((s) => s.key)).toEqual(['prepare#1', 'checks#1', 'checks#2', 'publish#1']);
    const clippy = sections[1].items.find((i) => i.kind === 'command');
    expect(clippy).toMatchObject({ command: 'make clippy', exit: 2 });
    expect(sections[0].failed).toBe(false);
    expect(sections[1].failed).toBe(true);
  });

  it('keeps the nextest summary', () => {
    const group = sections[2].items[0];
    expect(group.kind === 'command' && group.summary).toContain('1 failed');
  });

  it('collapses successful commands and expands failed ones', () => {
    const rows = flatten(sections, (_, d) => d);
    const prepareLines = rows.filter((r) => r.type === 'line' && r.line.step === 'prepare' && r.nested);
    expect(prepareLines).toHaveLength(0);
    expect(rows.some((r) => r.type === 'line' && r.line.line.startsWith('error[E0308]'))).toBe(true);
  });

  it('filters to matching lines', () => {
    const rows = flatten(sections, (_, d) => d, (l) => l.line.includes('E0308'));
    expect(rows.filter((r) => r.type === 'line').map((r) => r.type === 'line' && r.line.line)).toEqual([
      'error[E0308]: mismatched types',
    ]);
  });
});

describe('extractProblems', () => {
  const problems = extractProblems(buildSections([...ok, ...checks, ...tests, ...publish]));
  const titles = problems.map((p) => `${p.kind}: ${p.title}`);

  it('finds compiler diagnostics with locations', () => {
    expect(problems[0]).toMatchObject({
      kind: 'compile',
      title: 'this function has too many lines (108/100)',
      location: 'libs/foo/src/lib.rs:679:1',
    });
    expect(problems[1]).toMatchObject({ kind: 'compile', location: 'libs/foo/src/x.rs:10:5' });
  });

  it('finds failed tests once, with their output and panic location', () => {
    const failed = problems.filter((p) => p.kind === 'test');
    expect(failed).toHaveLength(1);
    expect(failed[0].title).toBe('test failed: cf-gears-foo::lib tests::broken');
    expect(failed[0].location).toBe('libs/foo/src/lib.rs:42:9');
    expect(failed[0].snippet.join('\n')).toContain('assertion `left == right` failed');
    expect(failed[0].snippet.join('\n')).not.toContain('tests::other');
  });

  it('recognises release-plz errors and rate limits', () => {
    expect(titles).toContain('error: crates.io rate limit (429): failed to release package');
  });

  it('reports failed steps and summaries after the details', () => {
    expect(titles).toContain('step: checks failed: checks_failed');
    expect(titles).toContain('summary: make: [clippy] Error 101');
    expect(titles).not.toContain('step: publish failed: crates_io_429, will retry in at least 115s');
    const kinds = problems.map((p) => p.kind);
    expect(kinds.indexOf('summary')).toBeGreaterThan(kinds.lastIndexOf('compile'));
  });

  it('falls back to the tail of an unrecognised failure', () => {
    const odd = lines('sync_main', 1, [
      ['cmd', '$ python3 /x/release_retry.py snapshot'],
      ['stderr', 'something odd happened'],
      ['info', 'exit 1'],
    ]);
    const found = extractProblems(buildSections(odd));
    expect(found[0]).toMatchObject({ kind: 'error', title: 'python3 /x/release_retry.py snapshot exited with 1' });
    expect(found[0].snippet).toEqual(['something odd happened']);
  });
});
