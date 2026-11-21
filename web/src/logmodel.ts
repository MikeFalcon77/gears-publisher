import type { LogLine } from './types';

/** One command: its `$ …` line, its output and how it ended. */
export interface CommandGroup {
  kind: 'command';
  key: string;
  step: string;
  attempt: number;
  command: string;
  header: LogLine;
  lines: LogLine[];
  /** `null` while running (or interrupted without an exit line). */
  exit: number | 'signal' | 'cancelled' | null;
  startedAt: string;
  endedAt?: string;
  /** nextest's `Summary` line, when present. */
  summary?: string;
}

/** A publisher note outside any command (`prepare: attempt 1 started`, …). */
export interface NoteItem {
  kind: 'note';
  line: LogLine;
}

/** All output of one attempt of one step. */
export interface Section {
  key: string;
  step: string;
  attempt: number;
  /** The release attempt it belongs to (see [`ReleaseAttempt`]). */
  release: number;
  items: (CommandGroup | NoteItem)[];
  failed: boolean;
}

/** What began a release attempt: the start of the release, or someone's Retry/Resume. */
export type AttemptTrigger = 'start' | 'retry' | 'resume';

/**
 * One execution of a release: from its start, or from a Retry/Resume, to the next one.
 * The engine's own repeats (interruptions, rate limits) stay inside an attempt.
 */
export interface ReleaseAttempt {
  number: number;
  trigger: AttemptTrigger;
  /** Who started it, from the publisher's control note. */
  by?: string;
  /** Who pressed Cancel during it. */
  cancelledBy?: string;
  startedAt: string;
  endedAt: string;
  sections: Section[];
}

/** The service's notes on the `control` pseudo-step (service.rs). */
const CONTROL = 'control';
const STARTED = /^(?:dry run|release) started by (.+)$/;
const REQUESTED = /^(retry|resume|cancel) requested by (.+)$/;

const EXIT = /^exit (\d+|by signal)$/;

export function failedExit(exit: CommandGroup['exit']): boolean {
  return exit !== null && exit !== 0;
}

/**
 * Splits the log into step attempts and commands, in arrival order. Lines are appended
 * one at a time, so a live log costs O(new lines) per update rather than a re-parse.
 * `sections` and the groups in it are mutated in place.
 */
export class LogModel {
  readonly sections: Section[] = [];
  /** Release attempts in order; the log's first line opens #1 if no control note did. */
  readonly attempts: ReleaseAttempt[] = [];
  private readonly byKey = new Map<string, Section>();
  private readonly open = new Map<string, CommandGroup>();

  append(line: LogLine): void {
    const release = this.release(line);
    release.endedAt = line.ts;
    // Control notes of each attempt form their own section; a step attempt never spans two.
    const key = line.step === CONTROL ? `${CONTROL}@${release.number}` : `${line.step}#${line.attempt}`;
    let section = this.byKey.get(key);
    if (!section) {
      section = { key, step: line.step, attempt: line.attempt, release: release.number, items: [], failed: false };
      this.byKey.set(key, section);
      this.sections.push(section);
      release.sections.push(section);
    }
    const current = this.open.get(key);
    if (line.stream === 'cmd') {
      const group: CommandGroup = {
        kind: 'command',
        key: `${key}@${line.seq}`,
        step: line.step,
        attempt: line.attempt,
        command: line.line.replace(/^\$ /, ''),
        header: line,
        lines: [],
        exit: null,
        startedAt: line.ts,
      };
      section.items.push(group);
      this.open.set(key, group);
      return;
    }
    if (current && line.stream === 'info') {
      const exit = EXIT.exec(line.line);
      if (exit || line.line.startsWith('cancelled:')) {
        current.exit = exit ? (exit[1] === 'by signal' ? 'signal' : Number(exit[1])) : 'cancelled';
        current.endedAt = line.ts;
        if (failedExit(current.exit)) section.failed = true;
        this.open.delete(key);
        return;
      }
    }
    if (current && (line.stream === 'stdout' || line.stream === 'stderr' || line.stream === 'info')) {
      current.lines.push(line);
      if (/^\s*Summary \[/.test(line.line)) current.summary = line.line.trim();
      return;
    }
    if (line.stream === 'info' && / failed \(/.test(line.line)) section.failed = true;
    section.items.push({ kind: 'note', line });
  }

  /** The attempt `line` belongs to; a Retry/Resume note opens a new one. */
  private release(line: LogLine): ReleaseAttempt {
    const open = (trigger: AttemptTrigger, by?: string) => {
      const attempt: ReleaseAttempt = {
        number: this.attempts.length + 1,
        trigger,
        by,
        startedAt: line.ts,
        endedAt: line.ts,
        sections: [],
      };
      this.attempts.push(attempt);
      return attempt;
    };
    const current = this.attempts[this.attempts.length - 1];
    if (line.step !== CONTROL) return current ?? open('start');
    const started = STARTED.exec(line.line);
    if (started) {
      // The service logs the start after the engine has begun, so step lines may come first.
      const first = this.attempts[0] ?? open('start');
      first.by = started[1];
      return first;
    }
    const requested = REQUESTED.exec(line.line);
    if (requested?.[1] === 'retry' || requested?.[1] === 'resume') return open(requested[1], requested[2]);
    const attempt = current ?? open('start');
    if (requested?.[1] === 'cancel') attempt.cancelledBy = requested[2];
    return attempt;
  }
}

/** How an earlier attempt ended, from its notes: the step that failed and why. */
export function attemptOutcome(attempt: ReleaseAttempt): { step?: string; error?: string; cancelled: boolean } {
  let failure: { step?: string; error?: string } = {};
  for (const section of attempt.sections) {
    for (const item of section.items) {
      if (item.kind !== 'note') continue;
      const failed = STEP_FAILED.exec(item.line.line);
      if (failed && !INTERRUPTED_CODES.has(failed[2])) failure = { step: failed[1], error: failed[2] };
    }
  }
  const last = [...attempt.sections].reverse().find((s) => s.step !== CONTROL);
  return { step: failure.step ?? last?.step, error: failure.error, cancelled: attempt.cancelledBy !== undefined };
}

export function buildSections(lines: LogLine[]): Section[] {
  const model = new LogModel();
  for (const line of lines) model.append(line);
  return model.sections;
}

/** `summary` lines (`make: ***`, `could not compile`) go to the run, not its findings. */
export type ProblemKind = 'compile' | 'test' | 'error' | 'deny' | 'summary';

/** One finding in the output of one command run. */
export interface Problem {
  id: string;
  kind: ProblemKind;
  title: string;
  /** cargo-deny's check (`vulnerability`, `unmaintained`, `banned`, …). */
  code?: string;
  /** `file:line:col` for compiler diagnostics. */
  location?: string;
  /** cargo-deny: the affected `crate version`s (one finding per advisory). */
  packages?: string[];
  /** cargo-deny: the advisory (`RUSTSEC-…`). */
  advisory?: string;
  step: string;
  attempt: number;
  /** Line to jump to. */
  seq: number;
  /** Context lines (the diagnostic, the test's output, the traceback…). */
  snippet: string[];
  /** The command that produced it. */
  command?: string;
}

// rustc/clippy (`error[E0308]`) and cargo-deny (`error[banned]`).
const RUST_DIAG = /^(error)(\[[\w-]+\])?: (.+)$/;
const RUST_SUMMARY = /^error: (could not compile|aborting due to|test failed|test run failed|failed to run)/;
// rustc's `-->` and cargo-deny's (codespan) `┌─`.
const LOCATION = /^\s*(?:-->|┌─) (\S+:\d+:\d+)/;
const NEXTEST_FAIL = /^\s*(FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|SIGBUS|LEAK-FAIL)\s+\[\s*[\d.]+s\]\s+(?:\(\s*\d+\/\s*\d+\)\s+)?(.+?)\s*$/;
const NEXTEST_STATUS = /^\s*(PASS|FAIL|SKIP|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|SIGBUS|LEAK|LEAK-FAIL|START|SLOW|RETRY|TRY \d+ \w+)\s+\[/;
const NEXTEST_SUMMARY = /^\s*(Summary|Cancelling|Starting)\s+\[/;
const MAKE_ERROR = /^make(\[\d+\])?: \*\*\* (.+)$/;
const ANYHOW = /^Error: (.+)$/;
const TRACING_ERROR = /^\S*\s*ERROR\s+(.+)$/;
const GH_ERROR = /^::error::(.+)$/;
const RATE_LIMIT = /429 Too Many Requests/;
const STEP_FAILED = /^(\w+): failed \((.+)\)$/;
const DENY_SUMMARY = /^(advisories|bans|licenses|sources) (ok|FAILED)(, (advisories|bans|licenses|sources) (ok|FAILED))*$/;
// cargo-deny's codespan excerpt of the Cargo.lock entry: `272 │ chrono 0.4.45 registry+…`.
const DENY_PACKAGE = /^\s*\d+\s*│\s*(\S+) (\d\S*) (?:registry|git|path)\+/;
const DENY_ADVISORY = /├ ID: (\S+)/;

const MAX_SNIPPET = 60;

/** The server's gap marker in condensed output (`make deny`). */
export const SKIPPED = /^… \d+ lines? skipped$/;

/** Explanations of the publisher's own failure codes. */
const STEP_HINTS: Record<string, string> = {
  checks_failed: 'a CI check failed; fix main, then press Retry',
  pr_closed: 'the release PR was closed without merging',
  registry_token_missing: 'CARGO_REGISTRY_TOKEN is not configured on the host',
  gh_not_logged_in: '`gh auth login` is needed on the host',
  publish_failed: 'release-plz release failed repeatedly',
  publish_dry_run_failed: 'the dry run failed before reaching an unpublished dependency',
  verify_mismatch: 'crates.io or tags do not match the manifests',
  crates_io_429: 'crates.io rate limit; the step waits and retries by itself',
  pr_open: 'the release PR is still open; polling continues',
};

type Found = Omit<Problem, 'id'>;

/** A finished command never changes, so what it contains is computed once. */
const scanned = new WeakMap<CommandGroup, Found[]>();

function commandProblems(group: CommandGroup): Found[] {
  const cached = scanned.get(group);
  if (cached) return cached;
  const found: Found[] = [];
  scanCommand(group, (p) => found.push(p));
  if (!found.some((p) => p.kind !== 'summary')) {
    // Nothing recognisable: show the end of the output.
    const tail = group.lines.slice(-40);
    found.push({
      kind: 'error',
      title: `${shortCommand(group.command)} exited with ${group.exit}`,
      step: group.step,
      attempt: group.attempt,
      seq: tail[0]?.seq ?? group.header.seq,
      snippet: tail.map((l) => l.line),
      command: group.command,
    });
  }
  scanned.set(group, found);
  return found;
}

/** A publisher note that a step attempt failed (`checks: failed (checks_failed)`). */
export interface StepNote {
  code: string;
  title: string;
  hint?: string;
  seq: number;
}

export type RunOutcome = 'failed' | 'interrupted' | 'ok' | 'running';

/** One execution of a command (or, for a step that failed outside any command, the step). */
export interface Run {
  key: string;
  /** 1-based position among the runs of its command. */
  ordinal: number;
  attempt: number;
  outcome: RunOutcome;
  /** Absent for a step failure outside any command. */
  group?: CommandGroup;
  seq: number;
  startedAt: string;
  endedAt?: string;
  problems: Problem[];
  /** `make: ***`, `could not compile …`, cargo-deny's and nextest's summaries. */
  summary: string[];
  notes: StepNote[];
  /** Ordinal of the later run with the same findings, when this one adds nothing. */
  sameAs?: number;
}

export type CommandStatus = 'failed' | 'retrying' | 'interrupted' | 'recovered';

/** Every run of one command in one step, for the commands that failed at least once. */
export interface CommandProblems {
  key: string;
  step: string;
  /** `null` for step failures outside any command. */
  command: string | null;
  /** All runs, oldest first, including successful ones. */
  runs: Run[];
  status: CommandStatus;
}

const INTERRUPTED_CODES = new Set(['cancelled', 'killed']);

/** cargo-deny checks that report a risk to watch rather than a known flaw. */
const MILD_DENY = new Set(['unmaintained', 'yanked', 'notice']);

export function isMildDeny(problem: Problem): boolean {
  return problem.kind === 'deny' && problem.code !== undefined && MILD_DENY.has(problem.code);
}

/**
 * The Problems tab: failed commands, each with its runs and their findings. Commands
 * whose last run failed come first, newest failure first.
 */
export function groupProblems(sections: Section[]): CommandProblems[] {
  const byKey = new Map<string, CommandProblems>();
  const entry = (step: string, command: string | null) => {
    const key = `${step}\u0000${command ?? ''}`;
    let found = byKey.get(key);
    if (!found) {
      found = { key, step, command, runs: [], status: 'failed' };
      byKey.set(key, found);
    }
    return found;
  };

  for (const section of sections) {
    let lastFailed: Run | undefined;
    for (const item of section.items) {
      if (item.kind === 'command') {
        const target = entry(section.step, item.command);
        const run = commandRun(item, target.runs.length + 1);
        target.runs.push(run);
        if (run.outcome === 'failed') lastFailed = run;
        continue;
      }
      const failed = STEP_FAILED.exec(item.line.line);
      if (!failed || failed[2] === 'pr_open' || INTERRUPTED_CODES.has(failed[2])) continue;
      const note: StepNote = {
        code: failed[2],
        title: `${failed[1]} failed: ${failed[2]}`,
        hint: STEP_HINTS[failed[2]],
        seq: item.line.seq,
      };
      if (lastFailed) {
        lastFailed.notes.push(note);
        continue;
      }
      const target = entry(section.step, null);
      target.runs.push({
        key: `${section.key}@${item.line.seq}`,
        ordinal: target.runs.length + 1,
        attempt: section.attempt,
        outcome: 'failed',
        seq: item.line.seq,
        startedAt: item.line.ts,
        endedAt: item.line.ts,
        problems: [],
        summary: [],
        notes: [note],
      });
    }
  }

  const result: CommandProblems[] = [];
  for (const command of byKey.values()) {
    const bad = command.runs.filter((r) => r.outcome === 'failed' || r.outcome === 'interrupted');
    if (!bad.length) continue;
    const last = command.runs[command.runs.length - 1];
    command.status =
      last.outcome === 'ok'
        ? 'recovered'
        : last.outcome === 'running'
          ? 'retrying'
          : bad.some((r) => r.outcome === 'failed')
            ? last.outcome === 'failed'
              ? 'failed'
              : 'interrupted'
            : 'interrupted';
    foldRepeats(bad);
    result.push(command);
  }
  const order: Record<CommandStatus, number> = { failed: 0, retrying: 1, interrupted: 2, recovered: 3 };
  const lastSeq = (c: CommandProblems) => c.runs[c.runs.length - 1].seq;
  return result.sort((a, b) => order[a.status] - order[b.status] || lastSeq(b) - lastSeq(a));
}

function commandRun(group: CommandGroup, ordinal: number): Run {
  const outcome: RunOutcome =
    group.exit === null ? 'running' : group.exit === 0 ? 'ok' : group.exit === 'cancelled' || group.exit === 'signal' ? 'interrupted' : 'failed';
  const run: Run = {
    key: group.key,
    ordinal,
    attempt: group.attempt,
    outcome,
    group,
    seq: group.header.seq,
    startedAt: group.startedAt,
    endedAt: group.endedAt,
    problems: [],
    summary: [],
    notes: [],
  };
  if (outcome !== 'failed') return run;
  const byId = new Map<string, Problem>();
  for (const p of commandProblems(group)) {
    if (p.kind === 'summary') {
      if (!run.summary.includes(p.title)) run.summary.push(p.title);
      continue;
    }
    // nextest repeats failures in its final list; cargo-deny reports an advisory once
    // per affected version of the crate.
    const id =
      p.kind === 'deny' && p.advisory
        ? `deny:${p.advisory}`
        : `${p.kind}:${p.title}:${p.kind === 'compile' ? (p.location ?? '') : ''}`;
    const known = byId.get(id);
    if (known) {
      for (const pkg of p.packages ?? []) if (!known.packages?.includes(pkg)) known.packages = [...(known.packages ?? []), pkg];
      continue;
    }
    byId.set(id, { ...p, id, packages: p.packages && [...p.packages] });
  }
  // Within cargo-deny, known flaws before risks to watch.
  const rank = (p: Problem) =>
    ({ compile: 0, test: 2, deny: 4, error: 6, summary: 8 })[p.kind] + (isMildDeny(p) ? 1 : 0);
  run.problems = [...byId.values()].sort((a, b) => rank(a) - rank(b) || a.seq - b.seq);
  if (group.summary) run.summary.push(group.summary);
  return run;
}

/** Marks each run whose findings equal the next (later) run's as the same as it. */
function foldRepeats(runs: Run[]) {
  const signature = (r: Run) =>
    r.outcome === 'interrupted'
      ? 'interrupted'
      : [...r.problems.map((p) => `${p.kind}:${p.title}`), ...r.notes.map((n) => `note:${n.code}`)].sort().join('\n');
  for (let i = runs.length - 2; i >= 0; i--) {
    const later = runs[i + 1];
    if (signature(runs[i]) === signature(later)) runs[i].sameAs = later.sameAs ?? later.ordinal;
  }
}

function scanCommand(group: CommandGroup, add: (p: Found) => void) {
  const lines = group.lines;
  const base = { step: group.step, attempt: group.attempt, command: group.command };
  for (let i = 0; i < lines.length; i++) {
    const text = lines[i].line;

    if (RUST_SUMMARY.test(text)) {
      add({ ...base, kind: 'summary', title: text.replace(/^error: /, ''), seq: lines[i].seq, snippet: [] });
      continue;
    }
    if (DENY_SUMMARY.test(text.trim())) {
      add({ ...base, kind: 'summary', title: text.trim(), seq: lines[i].seq, snippet: [] });
      continue;
    }
    const diag = RUST_DIAG.exec(text);
    if (diag) {
      const snippet = [text];
      let location: string | undefined;
      let pkg: RegExpExecArray | null = null;
      let advisory: string | undefined;
      let j = i + 1;
      for (; j < lines.length && snippet.length < MAX_SNIPPET; j++) {
        const next = lines[j].line;
        if (next.trim() === '' || RUST_DIAG.test(next) || /^(warning|error)\b/.test(next) || SKIPPED.test(next)) break;
        location ??= LOCATION.exec(next)?.[1];
        pkg ??= DENY_PACKAGE.exec(next);
        advisory ??= DENY_ADVISORY.exec(next)?.[1];
        snippet.push(next);
      }
      // cargo-deny's checks are words (`error[vulnerability]`); rustc's codes are `E0308`.
      const code = diag[2]?.slice(1, -1);
      const deny = code !== undefined && /^[a-z][a-z-]*$/.test(code);
      add({
        ...base,
        kind: deny ? 'deny' : 'compile',
        title: diag[3],
        code: deny ? code : undefined,
        location,
        packages: deny && pkg ? [`${pkg[1]} ${pkg[2]}`] : undefined,
        advisory: deny ? advisory : undefined,
        seq: lines[i].seq,
        snippet,
      });
      i = j - 1;
      continue;
    }

    const fail = NEXTEST_FAIL.exec(text);
    if (fail) {
      const snippet = [text];
      let j = i + 1;
      for (; j < lines.length && snippet.length < MAX_SNIPPET * 2; j++) {
        const next = lines[j].line;
        if (NEXTEST_STATUS.test(next) || NEXTEST_SUMMARY.test(next)) break;
        snippet.push(next);
      }
      const panic = snippet.find((l) => /panicked at|assertion .* failed|Error:/.test(l));
      add({
        ...base,
        kind: 'test',
        title: `${fail[1] === 'FAIL' ? 'test failed' : fail[1]}: ${fail[2]}`,
        location: panic ? /panicked at (\S+:\d+:\d+)/.exec(panic)?.[1] : undefined,
        seq: lines[i].seq,
        snippet,
      });
      i = j - 1;
      continue;
    }

    const make = MAKE_ERROR.exec(text);
    if (make) {
      add({ ...base, kind: 'summary', title: `make: ${make[2]}`, seq: lines[i].seq, snippet: [] });
      continue;
    }

    if (text.startsWith('Traceback (most recent call last):')) {
      const snippet = [text];
      let j = i + 1;
      for (; j < lines.length && snippet.length < MAX_SNIPPET; j++) {
        snippet.push(lines[j].line);
        if (!/^\s/.test(lines[j].line)) break;
      }
      add({ ...base, kind: 'error', title: snippet[snippet.length - 1], seq: lines[i].seq, snippet });
      i = j;
      continue;
    }

    const message = ANYHOW.exec(text)?.[1] ?? GH_ERROR.exec(text)?.[1] ?? TRACING_ERROR.exec(text)?.[1];
    if (message) {
      const snippet = [text];
      let j = i + 1;
      // anyhow's `Caused by:` chain and indented continuation lines.
      for (; j < lines.length && snippet.length < MAX_SNIPPET; j++) {
        const next = lines[j].line;
        if (!(next.trim() === '' || /^\s/.test(next) || next.startsWith('Caused by:'))) break;
        snippet.push(next);
      }
      while (snippet.length && snippet[snippet.length - 1].trim() === '') snippet.pop();
      const rateLimited = snippet.some((l) => RATE_LIMIT.test(l));
      add({
        ...base,
        kind: 'error',
        title: rateLimited ? `crates.io rate limit (429): ${message}` : message,
        seq: lines[i].seq,
        snippet,
      });
      i = j - 1;
    }
  }
}

/** `#2`, `#1–3`, `#1–2, 4`. */
export function runLabel(ordinals: number[]): string {
  const sorted = [...ordinals].sort((a, b) => a - b);
  const ranges: string[] = [];
  for (let i = 0; i < sorted.length; ) {
    let j = i;
    while (j + 1 < sorted.length && sorted[j + 1] === sorted[j] + 1) j++;
    ranges.push(i === j ? `${sorted[i]}` : `${sorted[i]}–${sorted[j]}`);
    i = j + 1;
  }
  return `#${ranges.join(', ')}`;
}

export function shortCommand(command: string): string {
  const parts = command.split(' ');
  const program = parts[0].split('/').pop() ?? parts[0];
  return [program, ...parts.slice(1, 3)].join(' ');
}

/** Rows of the virtual list, given which sections and commands are expanded. */
export type Row =
  | { type: 'section'; section: Section }
  | { type: 'command'; group: CommandGroup; open: boolean }
  | { type: 'line'; line: LogLine; nested: boolean; n?: number };

export function flatten(
  sections: Section[],
  isOpen: (key: string, defaultOpen: boolean) => boolean,
  match?: (line: LogLine) => boolean,
): Row[] {
  const rows: Row[] = [];
  for (const section of sections) {
    const visible = section.items.filter((item) =>
      !match ? true : item.kind === 'note' ? match(item.line) : match(item.header) || item.lines.some(match),
    );
    if (match && !visible.length) continue;
    rows.push({ type: 'section', section });
    if (!isOpen(section.key, true)) continue;
    for (const item of visible) {
      if (item.kind === 'note') {
        rows.push({ type: 'line', line: item.line, nested: false });
        continue;
      }
      // Failed and running commands start expanded; a search expands what matches.
      const open = isOpen(item.key, !!match || item.exit === null || failedExit(item.exit));
      rows.push({ type: 'command', group: item, open });
      if (!open) continue;
      item.lines.forEach((line, i) => {
        if (!match || match(line)) rows.push({ type: 'line', line, nested: true, n: i + 1 });
      });
    }
  }
  return rows;
}
