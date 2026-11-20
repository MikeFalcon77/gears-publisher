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
  items: (CommandGroup | NoteItem)[];
  failed: boolean;
}

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
  private readonly byKey = new Map<string, Section>();
  private readonly open = new Map<string, CommandGroup>();

  append(line: LogLine): void {
    const key = `${line.step}#${line.attempt}`;
    let section = this.byKey.get(key);
    if (!section) {
      section = { key, step: line.step, attempt: line.attempt, items: [], failed: false };
      this.byKey.set(key, section);
      this.sections.push(section);
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
}

export function buildSections(lines: LogLine[]): Section[] {
  const model = new LogModel();
  for (const line of lines) model.append(line);
  return model.sections;
}

export type ProblemKind = 'compile' | 'test' | 'error' | 'step' | 'summary';

export interface Problem {
  id: string;
  kind: ProblemKind;
  title: string;
  /** `file:line:col` for compiler diagnostics. */
  location?: string;
  step: string;
  attempt: number;
  /** Line to jump to. */
  seq: number;
  /** Context lines (the diagnostic, the test's output, the traceback…). */
  snippet: string[];
  /** The command that produced it. */
  command?: string;
}

const RUST_DIAG = /^(error)(\[[A-Z]\d{4}\])?: (.+)$/;
const RUST_SUMMARY = /^error: (could not compile|aborting due to|test failed|test run failed|failed to run)/;
const LOCATION = /^\s*--> (\S+:\d+:\d+)/;
const NEXTEST_FAIL = /^\s*(FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|SIGBUS|LEAK-FAIL)\s+\[\s*[\d.]+s\]\s+(?:\(\s*\d+\/\s*\d+\)\s+)?(.+?)\s*$/;
const NEXTEST_STATUS = /^\s*(PASS|FAIL|SKIP|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|SIGBUS|LEAK|LEAK-FAIL|START|SLOW|RETRY|TRY \d+ \w+)\s+\[/;
const NEXTEST_SUMMARY = /^\s*(Summary|Cancelling|Starting)\s+\[/;
const MAKE_ERROR = /^make(\[\d+\])?: \*\*\* (.+)$/;
const ANYHOW = /^Error: (.+)$/;
const TRACING_ERROR = /^\S*\s*ERROR\s+(.+)$/;
const GH_ERROR = /^::error::(.+)$/;
const RATE_LIMIT = /429 Too Many Requests/;
const STEP_FAILED = /^(\w+): failed \((.+)\)$/;

const MAX_SNIPPET = 60;

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
  if (!found.length) {
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

/** Problems in failed commands and failed steps, most useful first. */
export function extractProblems(sections: Section[]): Problem[] {
  const problems: Problem[] = [];
  const seen = new Set<string>();
  const add = (p: Found) => {
    // Tests are named once (nextest repeats failures in its final list).
    const where = p.kind === 'compile' ? (p.location ?? '') : '';
    const id = `${p.step}#${p.attempt}:${p.kind}:${p.title}:${where}`;
    if (seen.has(id)) return;
    seen.add(id);
    problems.push({ ...p, id });
  };

  for (const section of sections) {
    for (const item of section.items) {
      if (item.kind === 'note') {
        const failed = STEP_FAILED.exec(item.line.line);
        if (failed && failed[2] !== 'pr_open') {
          const hint = STEP_HINTS[failed[2]];
          add({
            kind: 'step',
            title: `${failed[1]} failed: ${failed[2]}`,
            step: section.step,
            attempt: section.attempt,
            seq: item.line.seq,
            snippet: hint ? [hint] : [],
          });
        }
        continue;
      }
      if (failedExit(item.exit)) commandProblems(item).forEach(add);
    }
  }
  const order: Record<ProblemKind, number> = { compile: 0, test: 1, error: 2, step: 3, summary: 4 };
  return problems.sort((a, b) => order[a.kind] - order[b.kind] || a.seq - b.seq);
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
    const diag = RUST_DIAG.exec(text);
    if (diag) {
      const snippet = [text];
      let location: string | undefined;
      let j = i + 1;
      for (; j < lines.length && snippet.length < MAX_SNIPPET; j++) {
        const next = lines[j].line;
        if (next.trim() === '' || RUST_DIAG.test(next) || /^(warning|error)\b/.test(next)) break;
        location ??= LOCATION.exec(next)?.[1];
        snippet.push(next);
      }
      add({ ...base, kind: 'compile', title: diag[3], location, seq: lines[i].seq, snippet });
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
