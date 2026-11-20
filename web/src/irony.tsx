import { useState, type ReactNode } from 'react';

/** The day GitHub flagged the constructorfabric organization and Actions stopped running
 * our release workflow. The reason this app exists. */
export const FLAGGED_SINCE = '2026-11-13';

export function daysSince(date: string, now = new Date()): number {
  const [y, m, d] = date.split('-').map(Number);
  const start = new Date(y, m - 1, d);
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  return Math.max(0, Math.round((today.getTime() - start.getTime()) / 86_400_000));
}

export const QUIPS = [
  'GitHub Actions, but artisanal, self-hosted and not flagged (yet).',
  "We didn't choose the self-hosted life. GitHub chose it for us.",
  'Runner: one VM. Flags: zero. Vibes: immaculate.',
  'Our GitHub Actions tab is a museum now.',
  'release-plz.yml, now in a trench coat.',
  'Powered by spite and PostgreSQL checkpoints.',
  'Every great CI system starts with a flagged organization.',
  'No YAML was harmed in the making of this button. Well, some.',
  'ubuntu-latest has left the chat. One VM stayed.',
  'Minutes billed by GitHub this month: 0. Coincidence? No.',
  'Workflow files: still in .github/. Just for nostalgia.',
  'It is not a workaround if it has a login page.',
  'Self-hosted runner, self-hosted UI, self-hosted coping.',
  'The appeal is pending. The release is not.',
  'Built in a week because a checkbox somewhere got ticked.',
];

/** One quip per mount, so it stays put while the page re-renders. */
export function useQuip(): string {
  const [quip] = useState(() => QUIPS[Math.floor(Math.random() * QUIPS.length)]);
  return quip;
}

export function Quip({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <p className={`text-xs text-slate-400 italic ${className}`}>{children}</p>;
}

export function UnflaggedBadge() {
  return (
    <span
      className="hidden items-center gap-1 rounded-full bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-800 ring-1 ring-emerald-200 ring-inset sm:inline-flex"
      title={`GitHub flagged constructorfabric on ${FLAGGED_SINCE}, so GitHub Actions stopped releasing gears-rust. This is our own CI with a button instead.`}
    >
      <span className="h-1.5 w-1.5 rounded-full bg-emerald-500" /> Self-hosted Actions™ · not flagged
    </span>
  );
}

export function FlaggedFooter() {
  const quip = useQuip();
  const days = daysSince(FLAGGED_SINCE);
  return (
    <footer className="border-t border-slate-200 bg-white">
      <div className="mx-auto flex max-w-[1600px] flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 text-xs text-slate-500">
        <span>
          Days since GitHub flagged constructorfabric: <b className="font-mono text-slate-700">{days}</b>
        </span>
        <span>
          Releases run by GitHub Actions since then: <b className="font-mono text-slate-700">0</b>
        </span>
        <span className="ml-auto italic text-slate-400">{quip}</span>
      </div>
    </footer>
  );
}
