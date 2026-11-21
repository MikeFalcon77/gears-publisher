import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  AlertTriangle,
  Bug,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CircleSlash,
  Copy,
  Download,
  FileCode2,
  FlaskConical,
  Info,
  ListTree,
  Loader2,
  RotateCw,
  LocateFixed,
  PackageCheck,
  Search,
  ShieldAlert,
  X,
  XCircle,
} from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Input } from '@/components/ui/input';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import { fullLog } from './api';
import {
  attemptOutcome,
  type AttemptTrigger,
  type ReleaseAttempt,
  groupProblems,
  isMildDeny,
  runLabel,
  type CommandProblems,
  type CommandStatus,
  type LogModel,
  type ProblemKind,
  type Run,
  failedExit,
  SKIPPED,
  flatten,
  type CommandGroup,
  type Problem,
  type Row,
  type Section,
} from './logmodel';
import { formatDuration, formatTime, statusLabel, tone } from './runview';
import { Badge as StatusBadge } from './ui';
import type { LogLine, RunState } from './types';

const ROW = 20;

type Tab = 'report' | 'problems' | 'log';

export interface LogSource {
  lines: LogLine[];
  model: LogModel;
  version: number;
  connection: 'connecting' | 'connected' | 'reconnecting' | 'idle';
  filter: string;
  setFilter: (step: string) => void;
}

export function LogViewer({
  runId,
  log,
  steps,
  state,
  report,
}: {
  runId: string;
  log: LogSource;
  steps: string[];
  /** The release's state, shown for its latest attempt. */
  state: RunState;
  /** What the release produced; adds a Report tab, opened when the release succeeds. */
  report?: ReactNode;
}) {
  // The model grows in place; `version` changes whenever it does. New arrays make the
  // memoised rows below recompute.
  const [sections, attempts] = useMemo(
    (): [Section[], ReleaseAttempt[]] => [[...log.model.sections], [...log.model.attempts]],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [log.model, log.version],
  );
  const [attempt, chooseAttempt] = useAttemptChoice(attempts.length);
  const visible = useMemo(
    () => (attempts.length > 1 ? sections.filter((s) => s.release === attempt) : sections),
    [sections, attempts.length, attempt],
  );
  const commands = useMemo(() => groupProblems(visible), [visible]);
  // Commands still failing; interrupted and recovered ones do not count.
  const failing = commands.filter((c) => c.status === 'failed' || c.status === 'retrying').length;
  // The report answers both "what was released" and "what does verify say is missing".
  const reportFirst =
    report !== undefined && (state.kind === 'succeeded' || ('error' in state && state.error === 'verify_mismatch'));
  const [tab, setTab] = useState<Tab>(() => (reportFirst ? 'report' : 'log'));
  const [jump, setJump] = useState<{ seq: number; at: number } | null>(null);

  // A failed run opens on what broke, a successful one on what it released; once, so
  // the viewer's own choice of tab sticks.
  const switched = useRef(false);
  const failed = state.kind === 'failed';
  useEffect(() => {
    if (switched.current) return;
    if (reportFirst) {
      switched.current = true;
      setTab('report');
    } else if (failed && failing) {
      switched.current = true;
      setTab('problems');
    }
  }, [failed, failing, reportFirst]);

  const showInLog = (seq: number) => {
    setTab('log');
    setJump({ seq, at: Date.now() });
  };

  return (
    <Tabs
      value={tab}
      onValueChange={(v) => setTab(v as Tab)}
      className="flex min-h-[60vh] flex-1 flex-col gap-0 overflow-hidden rounded-xl bg-white shadow-sm ring-1 ring-slate-200 lg:min-h-0"
    >
      {attempts.length > 1 && (
        <AttemptList attempts={attempts} selected={attempt} onSelect={chooseAttempt} state={state} />
      )}
      <div className="flex items-center gap-2 border-b border-slate-200 px-3 py-2">
        <TabsList>
          {report !== undefined && (
            <TabsTrigger value="report" className="gap-1.5">
              <PackageCheck className="size-4" /> Report
            </TabsTrigger>
          )}
          <TabsTrigger value="problems" className="gap-1.5">
            <Bug className="size-4" /> Problems
            {failing > 0 && (
              <Badge variant="destructive" className="h-5 min-w-5 px-1.5">
                {failing}
              </Badge>
            )}
          </TabsTrigger>
          <TabsTrigger value="log" className="gap-1.5">
            <ListTree className="size-4" /> Log
          </TabsTrigger>
        </TabsList>
        <span className="ml-auto text-xs text-slate-500">
          {log.connection === 'connected' ? (
            <span className="text-emerald-700">● live</span>
          ) : log.connection === 'reconnecting' ? (
            '○ reconnecting…'
          ) : log.connection === 'idle' ? (
            '○ finished'
          ) : (
            '○ connecting…'
          )}
        </span>
        <DownloadButton runId={runId} />
      </div>
      {report !== undefined && (
        <TabsContent value="report" className="min-h-0 flex-1 overflow-auto">
          {report}
        </TabsContent>
      )}
      <TabsContent value="problems" className="min-h-0 flex-1 overflow-auto">
        {/* Keyed by attempt: what is open in one attempt means nothing in another. */}
        <ProblemList key={attempt} commands={commands} onShow={showInLog} empty={log.lines.length === 0} />
      </TabsContent>
      <TabsContent value="log" className="flex min-h-0 flex-1 flex-col" forceMount hidden={tab !== 'log'}>
        <LogTree sections={visible} log={log} steps={steps} jump={jump} />
      </TabsContent>
    </Tabs>
  );
}

/**
 * The release attempt shown: the latest unless one was picked, so a new Retry comes
 * into view by itself. A pick is kept in `?attempt=` for sharing the link.
 */
function useAttemptChoice(count: number): [number, (n: number) => void] {
  const [picked, setPicked] = useState<number | null>(() => {
    const n = Number(new URLSearchParams(window.location.search).get('attempt'));
    return Number.isInteger(n) && n > 0 ? n : null;
  });
  const shown = picked !== null && picked <= count ? picked : count;
  const choose = (n: number) => {
    const next = n === count ? null : n;
    setPicked(next);
    const url = new URL(window.location.href);
    if (next === null) url.searchParams.delete('attempt');
    else url.searchParams.set('attempt', String(next));
    window.history.replaceState(null, '', url);
  };
  return [shown, choose];
}

const triggerLabel: Record<AttemptTrigger, string> = { start: 'Start', retry: 'Retry', resume: 'Resume' };

/** The release's attempts, newest first: its start and every Retry/Resume. */
function AttemptList({
  attempts,
  selected,
  onSelect,
  state,
}: {
  attempts: ReleaseAttempt[];
  selected: number;
  onSelect: (n: number) => void;
  state: RunState;
}) {
  return (
    <div className="border-b border-slate-200 px-2 py-2">
      <div className="px-2 pb-1 text-[11px] font-semibold tracking-wide text-slate-500 uppercase">Attempts</div>
      <ol className="max-h-36 overflow-auto">
        {[...attempts].reverse().map((a) => {
          const latest = a.number === attempts.length;
          return (
            <li key={a.number}>
              <button
                onClick={() => onSelect(a.number)}
                aria-current={a.number === selected}
                className={cn(
                  'flex w-full flex-wrap items-center gap-x-2 gap-y-0.5 rounded-md px-2 py-1 text-left text-xs hover:bg-slate-50',
                  a.number === selected && 'bg-sky-50 ring-1 ring-sky-200 hover:bg-sky-50',
                )}
              >
                <span className="font-mono font-semibold text-slate-900">#{a.number}</span>
                <span className="font-medium text-slate-700">{triggerLabel[a.trigger]}</span>
                <span className="text-slate-500" title={a.by}>
                  {formatTime(a.startedAt)}
                  {a.by && ` · ${a.by.split('@')[0]}`}
                </span>
                {a.mainSha && (
                  <span className="text-slate-500" title={a.mainSha}>
                    main <span className="font-mono">{a.mainSha.slice(0, 7)}</span>
                  </span>
                )}
                <span className="ml-auto flex items-center gap-2">
                  {latest ? <StatusBadge tone={tone(state)}>{statusLabel(state)}</StatusBadge> : <AttemptEnd attempt={a} />}
                  <span className="text-slate-500 tabular-nums">{formatDuration(a.startedAt, a.endedAt)}</span>
                </span>
              </button>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function AttemptEnd({ attempt }: { attempt: ReleaseAttempt }) {
  const end = attemptOutcome(attempt);
  if (end.cancelled)
    return <span className="text-slate-500">cancelled{attempt.cancelledBy && ` by ${attempt.cancelledBy.split('@')[0]}`}</span>;
  if (end.error)
    return (
      <span className="font-mono text-rose-700">
        {end.step}: {end.error}
      </span>
    );
  return <span className="text-slate-500">stopped in {end.step ?? '—'}</span>;
}

function DownloadButton({ runId }: { runId: string }) {
  const [busy, setBusy] = useState(false);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="size-8"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              const text = await fullLog(runId);
              const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
              const a = document.createElement('a');
              a.href = url;
              a.download = `run-${runId.slice(0, 8)}.log`;
              a.click();
              setTimeout(() => URL.revokeObjectURL(url), 10000);
            } finally {
              setBusy(false);
            }
          }}
        >
          {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
        </Button>
      </TooltipTrigger>
      <TooltipContent>Download the full log</TooltipContent>
    </Tooltip>
  );
}

// ---------------------------------------------------------------------------------
// Problems

type FindingKind = Exclude<ProblemKind, 'summary'>;

const kindMeta: Record<FindingKind, { label: string; icon: typeof Bug; className: string }> = {
  compile: { label: 'compile', icon: FileCode2, className: 'text-rose-700' },
  test: { label: 'test', icon: FlaskConical, className: 'text-rose-700' },
  error: { label: 'error', icon: XCircle, className: 'text-rose-700' },
  deny: { label: 'cargo-deny', icon: ShieldAlert, className: 'text-rose-700' },
};

function findingMeta(problem: Problem) {
  if (isMildDeny(problem) && problem.code)
    return { label: problem.code, icon: AlertTriangle, className: 'text-amber-600' };
  const meta = kindMeta[problem.kind as FindingKind];
  return problem.kind === 'deny' && problem.code ? { ...meta, label: problem.code } : meta;
}

const statusMeta: Record<CommandStatus, { icon: typeof Bug; className: string }> = {
  failed: { icon: XCircle, className: 'text-rose-700' },
  retrying: { icon: RotateCw, className: 'text-amber-600' },
  interrupted: { icon: CircleSlash, className: 'text-slate-400' },
  recovered: { icon: CheckCircle2, className: 'text-emerald-600' },
};

/** Runs that have something to show: failures and interruptions. */
function shownRuns(command: CommandProblems): Run[] {
  return command.runs.filter((r) => r.outcome === 'failed' || r.outcome === 'interrupted');
}

function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? '' : 's'}`;
}

/** `exit 2 · 8s · 7 findings`. The engine's step attempt is left out: next to the
 * release's attempts it reads as one of them. */
function runMeta(run: Run): string {
  const parts: string[] = [];
  if (run.outcome === 'interrupted') parts.push('interrupted');
  else if (run.group) parts.push(`exit ${run.group.exit}`);
  const took = formatDuration(run.startedAt, run.endedAt);
  if (took && run.group) parts.push(took);
  if (run.problems.length) parts.push(plural(run.problems.length, 'finding'));
  return parts.join(' · ');
}

function commandMeta(command: CommandProblems, shown: Run[]): string {
  const parts: string[] = [];
  const failed = shown.filter((r) => r.outcome === 'failed');
  const last = command.runs[command.runs.length - 1];
  if (shown.length === 1) parts.push(runMeta(shown[0]));
  else if (!failed.length) parts.push(`${plural(shown.length, 'interrupted run')} (${runLabel(shown.map((r) => r.ordinal))})`);
  else {
    parts.push(plural(shown.length, 'run'));
    if (failed.length < shown.length) parts.push(`${shown.length - failed.length} interrupted`);
  }
  if (command.status === 'recovered') parts.push(`ok in #${last.ordinal}`);
  if (command.status === 'retrying') parts.push(`running #${last.ordinal}`);
  return parts.join(' · ');
}

function ProblemList({
  commands,
  onShow,
  empty,
}: {
  commands: CommandProblems[];
  onShow: (seq: number) => void;
  empty: boolean;
}) {
  if (!commands.length)
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-8 text-sm text-slate-500">
        <CheckCircle2 className="size-8 text-emerald-500" />
        {empty ? 'No output yet.' : 'No problems in the output so far.'}
      </div>
    );
  return (
    <ul className="divide-y divide-slate-100">
      {commands.map((command, index) => (
        <CommandItem
          key={command.key}
          command={command}
          defaultOpen={index < 3 && (command.status === 'failed' || command.status === 'retrying')}
          onShow={onShow}
        />
      ))}
    </ul>
  );
}

function CommandItem({
  command,
  defaultOpen,
  onShow,
}: {
  command: CommandProblems;
  defaultOpen: boolean;
  onShow: (seq: number) => void;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const shown = shownRuns(command);
  const meta = statusMeta[command.status];
  const Icon = meta.icon;
  const muted = command.status === 'interrupted' || command.status === 'recovered';
  return (
    <li className="px-3 py-2.5">
      <Collapsible open={open} onOpenChange={setOpen}>
        <CollapsibleTrigger asChild>
          <button className="flex w-full items-start gap-2 text-left">
            <span className="mt-0.5 text-slate-400">
              {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </span>
            <Icon className={cn('mt-0.5 size-4 shrink-0', meta.className)} />
            <span className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-3 gap-y-0.5">
              <span className={cn('text-sm font-medium break-words', muted ? 'text-slate-500' : 'text-slate-900')}>
                {command.step} · <span className="font-mono">{command.command ?? 'step failed'}</span>
              </span>
              <span className="text-xs text-slate-500">{commandMeta(command, shown)}</span>
            </span>
          </button>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div className="mt-2 ml-6">
            {shown.length === 1 ? (
              <RunBody run={shown[0]} onShow={onShow} />
            ) : (
              <RunList runs={shown} onShow={onShow} />
            )}
          </div>
        </CollapsibleContent>
      </Collapsible>
    </li>
  );
}

/** Newest first; a run with the same findings as a later one folds under it. */
function RunList({ runs, onShow }: { runs: Run[]; onShow: (seq: number) => void }) {
  const leads = runs.filter((r) => r.sameAs === undefined).reverse();
  return (
    <ul className="space-y-2">
      {leads.map((lead, index) => (
        <RunItem
          key={lead.key}
          run={lead}
          repeats={runs.filter((r) => r.sameAs === lead.ordinal)}
          defaultOpen={index === 0}
          onShow={onShow}
        />
      ))}
    </ul>
  );
}

function RunItem({
  run,
  repeats,
  defaultOpen,
  onShow,
}: {
  run: Run;
  repeats: Run[];
  defaultOpen: boolean;
  onShow: (seq: number) => void;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [showRepeats, setShowRepeats] = useState(false);
  return (
    <li>
      <Collapsible open={open} onOpenChange={setOpen}>
        <CollapsibleTrigger asChild>
          <button className="flex w-full flex-wrap items-baseline gap-x-2 text-left text-xs text-slate-500 hover:text-slate-800">
            {open ? <ChevronDown className="size-3.5 self-center" /> : <ChevronRight className="size-3.5 self-center" />}
            <span className="font-mono font-medium text-slate-800">#{run.ordinal}</span>
            <span>{formatTime(run.startedAt)}</span>
            <span>{runMeta(run)}</span>
          </button>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div className="mt-1.5 ml-5">
            <RunBody run={run} onShow={onShow} />
          </div>
        </CollapsibleContent>
      </Collapsible>
      {repeats.length > 0 && (
        <div className="mt-1 text-xs text-slate-500">
          <button
            className="flex items-baseline gap-x-2 hover:text-slate-800"
            onClick={() => setShowRepeats((v) => !v)}
          >
            {showRepeats ? <ChevronDown className="size-3.5 self-center" /> : <ChevronRight className="size-3.5 self-center" />}
            <span className="font-mono font-medium text-slate-700">{runLabel(repeats.map((r) => r.ordinal))}</span>
            <span>
              same as #{run.ordinal}
              {repeats.length > 1 && ` (${plural(repeats.length, 'run')})`}
            </span>
          </button>
          {showRepeats && (
            <ul className="mt-1 ml-5 space-y-0.5">
              {[...repeats].reverse().map((r) => (
                <li key={r.key} className="flex flex-wrap items-center gap-x-2">
                  <span className="font-mono">#{r.ordinal}</span>
                  <span>{formatTime(r.startedAt)}</span>
                  <span>{runMeta(r)}</span>
                  <button className="text-sky-700 hover:underline" onClick={() => onShow(r.seq)}>
                    show in log
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </li>
  );
}

function RunBody({ run, onShow }: { run: Run; onShow: (seq: number) => void }) {
  return (
    <div className="space-y-2">
      {run.summary.length > 0 && (
        <div className="space-y-0.5 font-mono text-xs break-words text-slate-500">
          {run.summary.map((line) => (
            <div key={line}>{line}</div>
          ))}
        </div>
      )}
      {run.outcome === 'interrupted' && (
        <div className="flex flex-wrap items-center gap-2 text-xs text-slate-500">
          <span>Stopped before it finished: a service restart, a lost lease or Cancel. The engine runs the step again.</span>
          <Button variant="outline" size="sm" className="h-7 gap-1 text-xs" onClick={() => onShow(run.seq)}>
            <LocateFixed className="size-3.5" /> Show in log
          </Button>
        </div>
      )}
      {run.problems.length > 0 && (
        <ul className="divide-y divide-slate-100 rounded-md ring-1 ring-slate-100">
          {run.problems.map((problem) => (
            <ProblemItem
              key={problem.id}
              problem={problem}
              defaultOpen={run.problems.length <= 2}
              onShow={onShow}
            />
          ))}
        </ul>
      )}
      {run.notes.map((note) => (
        <div key={note.seq} className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-slate-600">
          <Info className="size-3.5 shrink-0 text-sky-700" />
          <span className="font-medium">{note.title}</span>
          {note.hint && <span className="text-slate-500">— {note.hint}</span>}
          {run.problems.length === 0 && (
            <Button variant="outline" size="sm" className="h-7 gap-1 text-xs" onClick={() => onShow(note.seq)}>
              <LocateFixed className="size-3.5" /> Show in log
            </Button>
          )}
        </div>
      ))}
    </div>
  );
}

function ProblemItem({
  problem,
  defaultOpen,
  onShow,
}: {
  problem: Problem;
  defaultOpen: boolean;
  onShow: (seq: number) => void;
}) {
  const meta = findingMeta(problem);
  const Icon = meta.icon;
  const [open, setOpen] = useState(defaultOpen && problem.snippet.length > 0);
  const where = problem.packages?.length
    ? [problem.packages.join(', '), problem.advisory].filter(Boolean).join(' · ')
    : problem.location;
  return (
    <li className="px-2.5 py-2">
      <Collapsible open={open} onOpenChange={setOpen}>
        <div className="flex items-start gap-2">
          <CollapsibleTrigger asChild disabled={!problem.snippet.length}>
            <button className="mt-0.5 text-slate-400 hover:text-slate-700 disabled:invisible" aria-label="Toggle details">
              {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </button>
          </CollapsibleTrigger>
          <Icon className={cn('mt-0.5 size-4 shrink-0', meta.className)} />
          <div className="min-w-0 flex-1">
            <div className="text-sm break-words text-slate-900">{problem.title}</div>
            <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-slate-500">
              <span>{meta.label}</span>
              {where && <span className="font-mono break-words text-slate-700">{where}</span>}
            </div>
          </div>
          <div className="flex shrink-0 gap-1">
            {problem.snippet.length > 0 && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-7"
                    onClick={() => void navigator.clipboard?.writeText(problem.snippet.join('\n'))}
                  >
                    <Copy className="size-3.5" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Copy</TooltipContent>
              </Tooltip>
            )}
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon" className="size-7" onClick={() => onShow(problem.seq)}>
                  <LocateFixed className="size-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Show in log</TooltipContent>
            </Tooltip>
          </div>
        </div>
        <CollapsibleContent>
          <pre className="mt-2 ml-12 max-h-80 overflow-auto rounded-md bg-slate-950 p-3 font-mono text-[12px] leading-[18px] text-slate-200">
            {problem.snippet.map((line, i) => (
              <div key={i} className={lineTone(line)}>
                {line || ' '}
              </div>
            ))}
          </pre>
        </CollapsibleContent>
      </Collapsible>
    </li>
  );
}

// ---------------------------------------------------------------------------------
// Log tree

function lineTone(text: string, stream?: LogLine['stream']): string {
  if (/^\s*(error(\[[\w-]+\])?:|FAIL |Error:|Caused by:|thread '.*' panicked|::error::)/.test(text) || /^make.*\*\*\*/.test(text))
    return 'text-rose-300';
  if (/^\s*warning(\[[\w-]+\])?:/.test(text)) return 'text-amber-300';
  if (stream === 'info' && SKIPPED.test(text)) return 'text-slate-500 italic';
  if (stream === 'info') return 'text-sky-300';
  if (stream === 'stderr') return 'text-slate-400';
  return 'text-slate-200';
}

function Highlight({ text, query }: { text: string; query: string }) {
  if (!query) return <>{text || ' '}</>;
  const lower = text.toLowerCase();
  const parts: React.ReactNode[] = [];
  let from = 0;
  for (let at = lower.indexOf(query); at >= 0; at = lower.indexOf(query, at + query.length)) {
    parts.push(text.slice(from, at), <mark key={at} className="rounded-sm bg-amber-300 text-slate-950">{text.slice(at, at + query.length)}</mark>);
    from = at + query.length;
  }
  parts.push(text.slice(from));
  return <>{parts}</>;
}

function LogTree({
  sections,
  log,
  steps,
  jump,
}: {
  sections: Section[];
  log: LogSource;
  steps: string[];
  jump: { seq: number; at: number } | null;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const [overrides, setOverrides] = useState<Map<string, boolean>>(new Map());
  const [query, setQuery] = useState('');
  const [follow, setFollow] = useState(true);
  const [flash, setFlash] = useState<number | null>(null);
  const needle = query.trim().toLowerCase();

  const visibleSections = useMemo(
    () => (log.filter === 'all' ? sections : sections.filter((s) => s.step === log.filter)),
    [sections, log.filter],
  );
  const rows = useMemo(
    () =>
      flatten(
        visibleSections,
        (key, open) => overrides.get(key) ?? open,
        needle ? (l) => l.line.toLowerCase().includes(needle) : undefined,
      ),
    [visibleSections, overrides, needle],
  );
  const matches = needle ? rows.filter((r) => r.type === 'line').length : 0;

  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW,
    overscan: 40,
  });

  const toggle = (key: string, current: boolean) =>
    setOverrides((m) => new Map(m).set(key, !current));

  useEffect(() => {
    if (follow && rows.length) virtual.scrollToIndex(rows.length - 1, { align: 'end' });
  }, [rows.length, follow, virtual]);

  // "Show in log": open the line's section and command, clear filters, then scroll.
  useEffect(() => {
    if (!jump) return;
    const section = sections.find((s) =>
      s.items.some((i) => (i.kind === 'note' ? i.line.seq === jump.seq : i.header.seq === jump.seq || i.lines.some((l) => l.seq === jump.seq))),
    );
    if (!section) return;
    const group = section.items.find(
      (i): i is CommandGroup => i.kind === 'command' && (i.header.seq === jump.seq || i.lines.some((l) => l.seq === jump.seq)),
    );
    setQuery('');
    setFollow(false);
    log.setFilter('all');
    setOverrides((m) => {
      const next = new Map(m).set(section.key, true);
      if (group) next.set(group.key, true);
      return next;
    });
    setFlash(jump.seq);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jump]);
  useEffect(() => {
    if (flash === null) return;
    const index = rows.findIndex((r) => r.type === 'line' && r.line.seq === flash);
    if (index >= 0) virtual.scrollToIndex(index, { align: 'center' });
    const timer = setTimeout(() => setFlash(null), 2500);
    return () => clearTimeout(timer);
  }, [flash, rows, virtual]);

  const expandAll = (open: boolean) => {
    const next = new Map<string, boolean>();
    for (const s of visibleSections) {
      next.set(s.key, true);
      for (const i of s.items) if (i.kind === 'command') next.set(i.key, open);
    }
    setOverrides(next);
  };

  return (
    <>
      <div className="flex flex-wrap items-center gap-2 border-b border-slate-200 bg-slate-50 px-3 py-2 text-xs text-slate-600">
        <select
          value={log.filter}
          onChange={(e) => log.setFilter(e.target.value)}
          className="h-8 rounded-md border border-slate-200 bg-white px-2 text-sm"
          aria-label="Step"
        >
          <option value="all">all steps</option>
          {[...steps, 'control'].map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <div className="relative">
          <Search className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-slate-400" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Find in log"
            className="h-8 w-56 bg-white pr-7 pl-7 text-sm"
          />
          {query && (
            <button className="absolute top-1/2 right-2 -translate-y-1/2 text-slate-400 hover:text-slate-700" onClick={() => setQuery('')}>
              <X className="size-3.5" />
            </button>
          )}
        </div>
        {needle && <span>{matches} matching lines</span>}
        <Button variant="ghost" size="sm" className="h-8" onClick={() => expandAll(true)}>
          Expand all
        </Button>
        <Button variant="ghost" size="sm" className="h-8" onClick={() => expandAll(false)}>
          Collapse all
        </Button>
        <label className="ml-auto flex items-center gap-1.5">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> follow
        </label>
      </div>
      <div
        ref={scroller}
        onWheel={(e) => e.deltaY < 0 && setFollow(false)}
        className="min-h-0 flex-1 overflow-auto bg-slate-950 font-mono text-[12px]"
      >
        {rows.length === 0 && <div className="p-3 text-slate-500">{needle ? 'No matching lines.' : 'No output yet.'}</div>}
        <div style={{ height: virtual.getTotalSize(), position: 'relative', minWidth: '100%', width: 'max-content' }}>
          {virtual.getVirtualItems().map((item) => (
            <div
              key={item.key}
              style={{ position: 'absolute', top: 0, left: 0, minWidth: '100%', height: ROW, transform: `translateY(${item.start}px)` }}
            >
              <RowView
                row={rows[item.index]}
                query={needle}
                flash={flash}
                onToggle={toggle}
                onContext={(seq) => {
                  setQuery('');
                  setFlash(seq);
                }}
              />
            </div>
          ))}
        </div>
      </div>
    </>
  );
}

function RowView({
  row,
  query,
  flash,
  onToggle,
  onContext,
}: {
  row: Row;
  query: string;
  flash: number | null;
  onToggle: (key: string, open: boolean) => void;
  onContext: (seq: number) => void;
}) {
  if (row.type === 'section') {
    const { section } = row;
    const running = section.items.some((i) => i.kind === 'command' && i.exit === null);
    return (
      <button
        onClick={() => onToggle(section.key, true)}
        className="sticky left-0 flex h-full w-full items-center gap-2 border-t border-slate-800 bg-slate-900 px-2 text-left font-sans text-[13px] font-semibold text-slate-100"
      >
        {running ? (
          <Loader2 className="size-3.5 animate-spin text-sky-400" />
        ) : section.failed ? (
          <XCircle className="size-3.5 text-rose-400" />
        ) : (
          <CheckCircle2 className="size-3.5 text-emerald-400" />
        )}
        <span className="font-mono">{section.step}</span>
        {section.attempt > 0 && <span className="font-normal text-slate-400">attempt {section.attempt}</span>}
      </button>
    );
  }
  if (row.type === 'command') {
    const { group, open } = row;
    const failed = failedExit(group.exit);
    return (
      <button
        onClick={() => onToggle(group.key, open)}
        className={cn(
          'flex h-full w-full items-center gap-2 pr-4 pl-4 text-left hover:bg-slate-800/70',
          failed ? 'text-rose-300' : 'text-sky-300',
        )}
      >
        {open ? <ChevronDown className="size-3.5 shrink-0" /> : <ChevronRight className="size-3.5 shrink-0" />}
        <span className="whitespace-pre">$ {group.command}</span>
        <span className="ml-2 flex shrink-0 items-center gap-2 font-sans text-[11px] text-slate-400">
          {group.exit === null ? (
            <Loader2 className="size-3 animate-spin" />
          ) : (
            <span className={failed ? 'text-rose-400' : 'text-emerald-400'}>
              {group.exit === 'cancelled' ? 'cancelled' : group.exit === 'signal' ? 'killed' : `exit ${group.exit}`}
            </span>
          )}
          <span>{formatDuration(group.startedAt, group.endedAt)}</span>
          <span>{group.lines.length} lines</span>
          {group.summary && <span className="text-slate-300">{group.summary.replace(/^Summary\s+/, '')}</span>}
        </span>
      </button>
    );
  }
  const { line, nested, n } = row;
  return (
    <div
      className={cn('group flex h-full items-center whitespace-pre', flash === line.seq && 'bg-amber-500/25')}
      onDoubleClick={query ? () => onContext(line.seq) : undefined}
      title={query ? 'Double-click: show in context' : undefined}
    >
      <span className={cn('sticky left-0 shrink-0 bg-slate-950 pr-3 text-right text-slate-600 select-none', nested ? 'w-16' : 'w-10')}>
        {n ?? ''}
      </span>
      <span className={lineTone(line.line, line.stream)}>
        <Highlight text={line.line} query={query} />
      </span>
    </div>
  );
}
