import { CheckCircle2, CircleDashed, ExternalLink, XCircle } from 'lucide-react';
import { formatDuration, formatTime, known, releaseReport, type ReportRow, type VerifyIssue } from './runview';
import type { RunDetail } from './types';

const issueText: Record<VerifyIssue, string> = {
  not_on_crates_io: 'not on crates.io',
  no_tag: 'no tag',
  lookup_error: 'crates.io lookup failed',
};

function StatusCell({ row }: { row: ReportRow }) {
  if (!row.verified)
    return (
      <span className="inline-flex items-center gap-1 whitespace-nowrap text-slate-500">
        <CircleDashed className="size-3.5" /> not verified yet
      </span>
    );
  if (!row.issues.length)
    return (
      <span className="inline-flex items-center gap-1 whitespace-nowrap text-emerald-700">
        <CheckCircle2 className="size-3.5" /> {row.gitOnly ? 'tag verified' : 'verified'}
      </span>
    );
  return (
    <span className="inline-flex flex-wrap items-center gap-x-2 gap-y-0.5 text-rose-700">
      {row.issues.map((issue) => (
        <span key={issue} className="inline-flex items-center gap-1 whitespace-nowrap">
          <XCircle className="size-3.5" /> {issueText[issue]}
        </span>
      ))}
    </span>
  );
}

function Link({ href, children }: { href?: string; children: React.ReactNode }) {
  if (!href) return <>{children}</>;
  return (
    <a href={href} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 text-sky-700 hover:underline">
      {children}
      <ExternalLink className="size-3" />
    </a>
  );
}

function Version({ row }: { row: ReportRow }) {
  return (
    <span className="font-mono">
      {row.previous && <span className="text-slate-500">{row.previous} → </span>}
      {row.version}
      {row.gitOnly ? (
        <span className="font-sans text-slate-500"> (git only)</span>
      ) : (
        !row.previous && row.previousKnown && <span className="font-sans text-slate-500"> (first)</span>
      )}
    </span>
  );
}

function Links({ row }: { row: ReportRow }) {
  return (
    <span className="flex gap-3">
      {row.crateUrl && <Link href={row.crateUrl}>crates.io</Link>}
      {row.tagUrl && <Link href={row.tagUrl}>tag</Link>}
      {row.compareUrl && <Link href={row.compareUrl}>changes</Link>}
    </span>
  );
}

/** What a finished (or publishing) release put on crates.io and GitHub. */
export function ReleaseReport({ run }: { run: RunDetail }) {
  const facts = run.record?.facts ?? {};
  const { rows, other } = releaseReport(facts);
  const state = run.progress.state;
  const finishedAt = state.kind === 'succeeded' ? known(state.completed_at) : undefined;
  const failed = rows.filter((r) => r.issues.length > 0).length;
  return (
    <div className="space-y-3 p-3 text-sm">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h2 className="text-base font-semibold">
          Released {rows.length} crate{rows.length === 1 ? '' : 's'}
        </h2>
        {facts.pr && <Link href={facts.pr.url}>PR #{facts.pr.number}</Link>}
        {facts.merge_sha && (
          <span className="text-slate-500">
            merged <span className="font-mono text-xs">{facts.merge_sha.slice(0, 12)}</span>
          </span>
        )}
        {finishedAt && (
          <span className="text-slate-500">
            {formatTime(finishedAt)} · took {formatDuration(run.progress.created_at, finishedAt)}
          </span>
        )}
        {failed > 0 && <span className="font-medium text-rose-700">{failed} not as expected</span>}
      </div>
      {/* Phones: one card per crate, the check next to the name. */}
      <ul className="divide-y divide-slate-100 rounded-lg text-xs ring-1 ring-slate-200 sm:hidden">
        {rows.map((row) => (
          <li key={row.name} className="space-y-1 px-3 py-2">
            <div className="flex flex-wrap items-baseline justify-between gap-x-2">
              <span className="font-mono text-slate-900">{row.name}</span>
              <StatusCell row={row} />
            </div>
            <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
              <Version row={row} />
              <Links row={row} />
            </div>
          </li>
        ))}
      </ul>
      <div className="hidden overflow-x-auto rounded-lg ring-1 ring-slate-200 sm:block">
        <table className="w-full text-left text-xs">
          <thead className="bg-slate-50 text-slate-500">
            <tr>
              <th className="px-3 py-2 font-medium">Crate</th>
              <th className="px-3 py-2 font-medium">Version</th>
              <th className="px-3 py-2 font-medium">Links</th>
              <th className="px-3 py-2 font-medium">Check</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-slate-100">
            {rows.map((row) => (
              <tr key={row.name}>
                <td className="px-3 py-1.5 font-mono whitespace-nowrap text-slate-900">{row.name}</td>
                <td className="px-3 py-1.5 whitespace-nowrap">
                  <Version row={row} />
                </td>
                <td className="px-3 py-1.5 whitespace-nowrap">
                  <Links row={row} />
                </td>
                <td className="px-3 py-1.5">
                  <StatusCell row={row} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {other.length > 0 && (
        <div className="text-xs">
          <p className="font-medium text-rose-700">Other differences found by verify</p>
          <ul className="mt-1 space-y-0.5 font-mono text-slate-700">
            {other.map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
