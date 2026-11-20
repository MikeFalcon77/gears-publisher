import { useEffect, useMemo, useRef, useState } from 'react';
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
  ListTree,
  Loader2,
  LocateFixed,
  Search,
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
  extractProblems,
  type LogModel,
  failedExit,
  flatten,
  type CommandGroup,
  type Problem,
  type Row,
  type Section,
} from './logmodel';
import { formatDuration } from './runview';
import type { LogLine } from './types';

const ROW = 20;

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
  failed,
}: {
  runId: string;
  log: LogSource;
  steps: string[];
  failed: boolean;
}) {
  // The model grows in place; `version` changes whenever it does. A new array makes
  // the memoised rows below recompute.
  const sections = useMemo(
    () => [...log.model.sections],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [log.model, log.version],
  );
  const problems = useMemo(() => extractProblems(sections), [sections]);
  const [tab, setTab] = useState<'problems' | 'log'>('log');
  const [jump, setJump] = useState<{ seq: number; at: number } | null>(null);

  // A failed run opens on what broke, once.
  const switched = useRef(false);
  useEffect(() => {
    if (!switched.current && failed && problems.length) {
      switched.current = true;
      setTab('problems');
    }
  }, [failed, problems.length]);

  const showInLog = (seq: number) => {
    setTab('log');
    setJump({ seq, at: Date.now() });
  };

  return (
    <Tabs
      value={tab}
      onValueChange={(v) => setTab(v as 'problems' | 'log')}
      className="flex min-h-[60vh] flex-1 flex-col gap-0 overflow-hidden rounded-xl bg-white shadow-sm ring-1 ring-slate-200 lg:min-h-0"
    >
      <div className="flex items-center gap-2 border-b border-slate-200 px-3 py-2">
        <TabsList>
          <TabsTrigger value="problems" className="gap-1.5">
            <Bug className="size-4" /> Problems
            {problems.length > 0 && (
              <Badge variant="destructive" className="h-5 min-w-5 px-1.5">
                {problems.length}
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
      <TabsContent value="problems" className="min-h-0 flex-1 overflow-auto">
        <ProblemList problems={problems} onShow={showInLog} empty={log.lines.length === 0} />
      </TabsContent>
      <TabsContent value="log" className="flex min-h-0 flex-1 flex-col" forceMount hidden={tab !== 'log'}>
        <LogTree sections={sections} log={log} steps={steps} jump={jump} />
      </TabsContent>
    </Tabs>
  );
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

const kindMeta: Record<Problem['kind'], { label: string; icon: typeof Bug; className: string }> = {
  compile: { label: 'compile', icon: FileCode2, className: 'text-rose-700' },
  test: { label: 'test', icon: FlaskConical, className: 'text-rose-700' },
  error: { label: 'error', icon: XCircle, className: 'text-rose-700' },
  step: { label: 'step', icon: AlertTriangle, className: 'text-amber-700' },
  summary: { label: 'summary', icon: CircleSlash, className: 'text-slate-500' },
};

function ProblemList({
  problems,
  onShow,
  empty,
}: {
  problems: Problem[];
  onShow: (seq: number) => void;
  empty: boolean;
}) {
  if (!problems.length)
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-8 text-sm text-slate-500">
        <CheckCircle2 className="size-8 text-emerald-500" />
        {empty ? 'No output yet.' : 'No problems in the output so far.'}
      </div>
    );
  return (
    <ul className="divide-y divide-slate-100">
      {problems.map((problem, index) => (
        <ProblemItem key={problem.id} problem={problem} defaultOpen={index < 3} onShow={onShow} />
      ))}
    </ul>
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
  const meta = kindMeta[problem.kind];
  const Icon = meta.icon;
  const [open, setOpen] = useState(defaultOpen && problem.snippet.length > 0);
  return (
    <li className="px-3 py-2.5">
      <Collapsible open={open} onOpenChange={setOpen}>
        <div className="flex items-start gap-2">
          <CollapsibleTrigger asChild disabled={!problem.snippet.length}>
            <button className="mt-0.5 text-slate-400 hover:text-slate-700 disabled:invisible" aria-label="Toggle details">
              {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </button>
          </CollapsibleTrigger>
          <Icon className={cn('mt-0.5 size-4 shrink-0', meta.className)} />
          <div className="min-w-0 flex-1">
            <div className="text-sm font-medium break-words text-slate-900">{problem.title}</div>
            <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-slate-500">
              <Badge variant="outline" className="font-mono">
                {problem.step}
                {problem.attempt > 1 && ` #${problem.attempt}`}
              </Badge>
              <span>{meta.label}</span>
              {problem.location && <span className="font-mono text-slate-700">{problem.location}</span>}
              {problem.command && <span className="truncate font-mono">$ {problem.command}</span>}
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
            <Button variant="outline" size="sm" className="h-7 gap-1 text-xs" onClick={() => onShow(problem.seq)}>
              <LocateFixed className="size-3.5" /> Show in log
            </Button>
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
  if (/^\s*(error(\[\w+\])?:|FAIL |Error:|Caused by:|thread '.*' panicked|::error::)/.test(text) || /^make.*\*\*\*/.test(text))
    return 'text-rose-300';
  if (/^\s*warning(\[\w+\])?:/.test(text)) return 'text-amber-300';
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
