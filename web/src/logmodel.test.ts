import { describe, expect, it } from 'vitest';
import { attemptOutcome, buildSections, flatten, groupProblems, LogModel, runLabel, type CommandProblems } from './logmodel';
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

/** Findings of each command's latest failed run, as `step · command: kind: title`. */
function latestFindings(commands: CommandProblems[]): string[] {
  return commands.flatMap((c) => {
    const run = [...c.runs].reverse().find((r) => r.outcome === 'failed');
    return (run?.problems ?? []).map((p) => `${c.step} · ${c.command}: ${p.kind}: ${p.title}`);
  });
}

describe('groupProblems', () => {
  const commands = groupProblems(buildSections([...ok, ...checks, ...tests, ...publish]));
  const byCommand = (command: string) => commands.find((c) => c.command === command)!;

  it('groups findings under the command that printed them', () => {
    expect(commands.map((c) => `${c.step} · ${c.command}`)).toEqual([
      'publish · release-plz release -o json',
      'checks · make test-no-macros',
      'checks · make clippy',
    ]);
    expect(commands.find((c) => c.step === 'prepare')).toBeUndefined();
  });

  it('finds compiler diagnostics with locations', () => {
    const [first, second] = byCommand('make clippy').runs[0].problems;
    expect(first).toMatchObject({
      kind: 'compile',
      title: 'this function has too many lines (108/100)',
      location: 'libs/foo/src/lib.rs:679:1',
    });
    expect(second).toMatchObject({ kind: 'compile', location: 'libs/foo/src/x.rs:10:5' });
  });

  it('finds failed tests once, with their output and panic location', () => {
    const failed = byCommand('make test-no-macros').runs[0].problems.filter((p) => p.kind === 'test');
    expect(failed).toHaveLength(1);
    expect(failed[0].title).toBe('test failed: cf-gears-foo::lib tests::broken');
    expect(failed[0].location).toBe('libs/foo/src/lib.rs:42:9');
    expect(failed[0].snippet.join('\n')).toContain('assertion `left == right` failed');
    expect(failed[0].snippet.join('\n')).not.toContain('tests::other');
  });

  it('recognises release-plz errors and rate limits', () => {
    expect(latestFindings(commands)).toContain(
      'publish · release-plz release -o json: error: crates.io rate limit (429): failed to release package',
    );
  });

  it('keeps summaries and the step failure with the run, not among the findings', () => {
    const run = byCommand('make clippy').runs[0];
    expect(run.summary).toEqual(['could not compile `cf-gears-foo` (lib) due to 2 previous errors', 'make: [clippy] Error 101']);
    expect(run.notes.map((n) => n.title)).toEqual(['checks failed: checks_failed']);
    expect(run.problems.every((p) => p.kind !== 'summary')).toBe(true);
  });

  it('folds a retry with the same findings into the later run', () => {
    const clippy = (attempt: number) =>
      lines('checks', attempt, [
        ['cmd', '$ make clippy'],
        ['stderr', 'error: item in documentation is missing backticks'],
        ['stderr', '  --> libs/foo/src/models.rs:70:5'],
        ['stderr', ''],
        ['info', 'exit 2'],
        ['info', 'checks: failed (checks_failed)'],
      ]);
    const [command] = groupProblems(buildSections([...clippy(1), ...clippy(2)]));
    expect(command.status).toBe('failed');
    expect(command.runs.map((r) => [r.ordinal, r.attempt, r.sameAs])).toEqual([
      [1, 1, 2],
      [2, 2, undefined],
    ]);
  });

  it('folds only consecutive repeats and keeps a different failure visible', () => {
    const run = (attempt: number, error: string) =>
      lines('publish', attempt, [
        ['cmd', '$ release-plz release'],
        ['stderr', `Error: ${error}`],
        ['info', 'exit 1'],
      ]);
    const [command] = groupProblems(
      buildSections([...run(1, 'tag push failed'), ...run(2, 'tag push failed'), ...run(3, '429'), ...run(4, '429'), ...run(5, '429')]),
    );
    expect(command.runs.map((r) => r.sameAs)).toEqual([2, undefined, 5, 5, undefined]);
  });

  it('shows cargo-deny findings once per advisory, with crates and ids', () => {
    const advisory = (lock: number, crate: string, id: string, title: string, code = 'vulnerability') =>
      [
        ['stderr', `error[${code}]: ${title}`],
        ['stderr', `    ┌─ /src/Cargo.lock:${lock}:1`],
        ['stderr', '    │'],
        ['stderr', `${lock} │ ${crate} registry+https://github.com/rust-lang/crates.io-index`],
        ['stderr', '    │ ━━━━━━━━━━━━━━━━━━━━ security vulnerability detected'],
        ['stderr', '    │'],
        ['stderr', `    ├ ID: ${id}`],
        ['stderr', '    ├ Solution: Upgrade to >=9.0.0 (try `cargo update -p zip`)'],
        ['stderr', ''],
      ] as [LogLine['stream'], string][];
    const deny = lines('checks', 3, [
      ['cmd', '$ make deny'],
      ['info', 'output condensed to errors and warnings; run `make deny` locally for all of it'],
      ...advisory(272, 'chrono 0.4.45', 'RUSTSEC-2026-0543', 'chrono is unmaintained', 'unmaintained'),
      ...advisory(1082, 'zip 4.6.1', 'RUSTSEC-2026-0545', 'AES entries returned without checking'),
      ...advisory(1083, 'zip 8.6.0', 'RUSTSEC-2026-0545', 'AES entries returned without checking'),
      ['info', '… 120 lines skipped'],
      ['stderr', 'advisories FAILED, bans ok, licenses ok, sources ok'],
      ['stderr', 'make: *** [.gears-dna/gears.mk:754: deny] Error 1'],
      ['info', 'exit 2'],
      ['info', 'checks: failed (checks_failed)'],
    ]);
    const [command] = groupProblems(buildSections(deny));
    const run = command.runs[0];
    expect(run.summary).toEqual(['advisories FAILED, bans ok, licenses ok, sources ok', 'make: [.gears-dna/gears.mk:754: deny] Error 1']);
    // Vulnerabilities first, though chrono came first in the output.
    expect(run.problems.map((p) => [p.code, p.title, p.packages, p.advisory])).toEqual([
      ['vulnerability', 'AES entries returned without checking', ['zip 4.6.1', 'zip 8.6.0'], 'RUSTSEC-2026-0545'],
      ['unmaintained', 'chrono is unmaintained', ['chrono 0.4.45'], 'RUSTSEC-2026-0543'],
    ]);
    expect(run.problems.every((p) => p.kind === 'deny')).toBe(true);
  });

  it('lists cargo-deny errors, not its warnings', () => {
    const deny = lines('checks', 1, [
      ['cmd', '$ make deny'],
      ['stderr', "warning[duplicate]: found 2 duplicate entries for crate 'syn'"],
      ['stderr', '   ┌─ /src/Cargo.lock:10:1'],
      ['stderr', ''],
      ['info', '… 120 lines skipped'],
      ['stderr', "error[banned]: crate 'openssl = 0.10.64' is explicitly banned"],
      ['stderr', '   ┌─ /src/Cargo.lock:42:1'],
      ['stderr', '   │'],
      ['stderr', ''],
      ['stderr', 'advisories ok, bans FAILED, licenses ok, sources ok'],
      ['info', 'exit 4'],
    ]);
    const [run] = groupProblems(buildSections(deny))[0].runs;
    expect(run.problems).toHaveLength(1);
    expect(run.problems[0]).toMatchObject({
      kind: 'deny',
      code: 'banned',
      title: "crate 'openssl = 0.10.64' is explicitly banned",
      location: '/src/Cargo.lock:42:1',
    });
  });

  it('treats an interrupted run as no finding, and a later success as recovered', () => {
    const log = [
      ...lines('checks', 1, [
        ['cmd', '$ make clippy'],
        ['stderr', '    Checking foo'],
        ['info', 'cancelled: process group terminated'],
        ['info', 'checks: failed (cancelled)'],
      ]),
      ...lines('checks', 2, [
        ['cmd', '$ make clippy'],
        ['info', 'cancelled: process group terminated'],
        ['info', 'checks: failed (cancelled)'],
      ]),
      ...lines('prepare', 1, [['cmd', '$ git fetch origin'], ['info', 'exit 128']]),
      ...lines('prepare', 2, [['cmd', '$ git fetch origin'], ['info', 'exit 0']]),
    ];
    const commands = groupProblems(buildSections(log));
    const clippy = commands.find((c) => c.command === 'make clippy')!;
    expect(clippy.status).toBe('interrupted');
    expect(clippy.runs.map((r) => [r.outcome, r.problems.length, r.notes.length, r.sameAs])).toEqual([
      ['interrupted', 0, 0, 2],
      ['interrupted', 0, 0, undefined],
    ]);
    expect(commands.find((c) => c.command === 'git fetch origin')!.status).toBe('recovered');
  });

  it('reports a step that failed outside any command, as the server words it', () => {
    // The wrapper writes `, will retry` after every retryable failure, the last one too.
    const verify = (attempt: number) =>
      lines('verify', attempt, [
        ['info', `verify: attempt ${attempt} started`],
        ['info', 'verify: failed (verify_mismatch), will retry'],
      ]);
    const [command] = groupProblems(buildSections([...verify(1), ...verify(2)]));
    expect(command).toMatchObject({ step: 'verify', command: null, status: 'failed' });
    expect(command.runs.map((r) => r.sameAs)).toEqual([2, undefined]);
    expect(command.runs[1].notes[0]).toMatchObject({ code: 'verify_mismatch', hint: 'crates.io or tags do not match the manifests' });

    const failedFirst = verify(1);
    const later = lines('verify', 3, [['info', 'verify: done']]);
    expect(groupProblems(buildSections([...failedFirst, ...later]))[0].status).toBe('recovered');
  });

  it('keeps runs apart when the same error is in another place', () => {
    const clippy = (attempt: number, file: string) =>
      lines('checks', attempt, [
        ['cmd', '$ make clippy'],
        ['stderr', 'error[E0308]: mismatched types'],
        ['stderr', `  --> ${file}:1:1`],
        ['stderr', ''],
        ['info', 'exit 2'],
      ]);
    const [command] = groupProblems(buildSections([...clippy(1, 'a.rs'), ...clippy(2, 'b.rs')]));
    expect(command.runs.map((r) => r.sameAs)).toEqual([undefined, undefined]);
  });

  it('falls back to the tail of an unrecognised failure', () => {
    const odd = lines('sync_main', 1, [
      ['cmd', '$ python3 /x/release_retry.py snapshot'],
      ['stderr', 'something odd happened'],
      ['info', 'exit 1'],
    ]);
    const [problem] = groupProblems(buildSections(odd))[0].runs[0].problems;
    expect(problem).toMatchObject({ kind: 'error', title: 'python3 /x/release_retry.py snapshot exited with 1' });
    expect(problem.snippet).toEqual(['something odd happened']);
  });
});

describe('runLabel', () => {
  it('names runs compactly', () => {
    expect(runLabel([2])).toBe('#2');
    expect(runLabel([3, 1, 2])).toBe('#1–3');
    expect(runLabel([1, 2, 4])).toBe('#1–2, 4');
  });
});

describe('release attempts', () => {
  const model = (all: LogLine[]) => {
    const m = new LogModel();
    all.forEach((l) => m.append(l));
    return m;
  };
  const clippyFails = (attempt: number) =>
    lines('checks', attempt, [
      ['info', `checks: attempt ${attempt} started`],
      ['cmd', '$ make clippy'],
      ['stderr', 'error: item in documentation is missing backticks'],
      ['info', 'exit 2'],
      ['info', 'checks: failed (checks_failed)'],
    ]);

  it('opens an attempt per start and Retry, and keeps step output in its attempt', () => {
    const m = model([
      // The engine's first lines can arrive before the service's start note.
      ...lines('prepare', 1, [['info', 'prepare: attempt 1 started'], ['info', 'prepare: done']]),
      ...lines('control', 0, [['info', 'dry run started by a@example.com']]),
      ...clippyFails(1),
      ...lines('control', 0, [['info', 'retry requested by b@example.com']]),
      ...clippyFails(2),
    ]);
    expect(m.attempts.map((a) => [a.number, a.trigger, a.by])).toEqual([
      [1, 'start', 'a@example.com'],
      [2, 'retry', 'b@example.com'],
    ]);
    expect(m.attempts.map((a) => a.sections.map((s) => s.key))).toEqual([
      ['prepare#1', 'control@1', 'checks#1'],
      ['control@2', 'checks#2'],
    ]);
    expect(m.sections.map((s) => s.release)).toEqual([1, 1, 1, 2, 2]);
    expect(attemptOutcome(m.attempts[0])).toEqual({ step: 'checks', error: 'checks_failed', cancelled: false });
  });

  it('records the main commit each attempt checked', () => {
    const m = model([
      ...lines('prepare', 1, [['info', 'main is at 45f40590b262d79b8e59aec8eb7a62098d6d7ee9']]),
      ...clippyFails(1),
      ...lines('control', 0, [['info', 'retry requested by b@example.com']]),
      ...lines('checks', 2, [['info', 'main moved: 45f40590b262d79b8e59aec8eb7a62098d6d7ee9 → a4c53672c12d830f6afbd1819fcf644ae2543b0a; checking a4c53672c12d830f6afbd1819fcf644ae2543b0a']]),
    ]);
    expect(m.attempts.map((a) => a.mainSha?.slice(0, 7))).toEqual(['45f4059', 'a4c5367']);
    // A Retry with no new commits stays on the commit before it.
    m.append(lines('control', 0, [['info', 'retry requested by b@example.com']])[0]);
    expect(m.attempts[2].mainSha?.slice(0, 7)).toBe('a4c5367');
  });

  it('folds a Retry the engine refused back into the attempt before it', () => {
    const m = model([
      ...lines('control', 0, [['info', 'release started by a@example.com']]),
      ...clippyFails(1),
      ...lines('control', 0, [
        ['info', 'retry requested by b@example.com'],
        ['info', 'retry rejected: run is not failed'],
      ]),
    ]);
    expect(m.attempts).toHaveLength(1);
    expect(m.sections.map((s) => s.key)).toEqual(['control@1', 'checks#1']);
    expect(m.sections[0].items).toHaveLength(3);
    // A later Retry that goes through is attempt #2 again.
    m.append(lines('control', 0, [['info', 'retry requested by b@example.com']])[0]);
    expect(m.attempts.map((a) => a.number)).toEqual([1, 2]);
  });

  it('reports the last failure of an attempt, unless the step later finished', () => {
    const m = model([
      ...lines('prepare', 1, [['info', 'prepare: failed (git_fetch_failed), will retry']]),
      ...lines('prepare', 2, [['info', 'prepare: done']]),
      ...lines('checks', 1, [['info', 'checks: attempt 1 started']]),
    ]);
    expect(attemptOutcome(m.attempts[0])).toEqual({ step: 'checks', error: undefined, cancelled: false });
  });

  it('keeps one attempt when nothing was retried, and notes a Cancel', () => {
    const m = model([
      ...lines('control', 0, [['info', 'release started by a@example.com']]),
      ...lines('checks', 1, [
        ['cmd', '$ make clippy'],
        ['info', 'cancelled: process group terminated'],
        ['info', 'checks: failed (cancelled)'],
      ]),
      ...lines('control', 0, [['info', 'cancel requested by c@example.com']]),
      ...lines('control', 0, [['info', 'resume requested by c@example.com']]),
      ...lines('checks', 2, [['info', 'checks: attempt 2 started']]),
    ]);
    expect(m.attempts.map((a) => [a.trigger, a.cancelledBy])).toEqual([
      ['start', 'c@example.com'],
      ['resume', undefined],
    ]);
    // An interruption is not a failure of the step.
    expect(attemptOutcome(m.attempts[0])).toEqual({ step: 'checks', error: undefined, cancelled: true });
  });
});
