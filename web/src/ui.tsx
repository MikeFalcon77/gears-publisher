import type { ButtonHTMLAttributes, ReactNode } from 'react';
import { CheckCircle2, CircleDashed, Clock, Loader2, MinusCircle, PauseCircle, XCircle } from 'lucide-react';
import type { StepStatus, Tone } from './runview';

const tones: Record<Tone, string> = {
  neutral: 'bg-slate-100 text-slate-700 ring-slate-200',
  active: 'bg-sky-50 text-sky-800 ring-sky-200',
  good: 'bg-emerald-50 text-emerald-800 ring-emerald-200',
  bad: 'bg-rose-50 text-rose-800 ring-rose-200',
  warn: 'bg-amber-50 text-amber-800 ring-amber-200',
};

export function Badge({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <span className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${tones[tone]}`}>
      {children}
    </span>
  );
}

type Variant = 'primary' | 'secondary' | 'danger';
const variants: Record<Variant, string> = {
  primary: 'bg-slate-900 text-white hover:bg-slate-700 disabled:bg-slate-400',
  secondary: 'bg-white text-slate-800 ring-1 ring-inset ring-slate-300 hover:bg-slate-50 disabled:text-slate-400',
  danger: 'bg-rose-600 text-white hover:bg-rose-500 disabled:bg-rose-300',
};

export function Button({
  variant = 'secondary',
  className = '',
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      type="button"
      className={`inline-flex items-center justify-center gap-1.5 rounded-md px-3 py-1.5 text-sm font-medium transition disabled:cursor-not-allowed ${variants[variant]} ${className}`}
      {...props}
    />
  );
}

export function StepIcon({ status }: { status: StepStatus }) {
  const size = 'h-5 w-5 shrink-0';
  switch (status) {
    case 'running':
      return <Loader2 className={`${size} animate-spin text-sky-600`} aria-label="running" />;
    case 'retry_wait':
      return <Clock className={`${size} text-amber-600`} aria-label="waiting to retry" />;
    case 'succeeded':
      return <CheckCircle2 className={`${size} text-emerald-600`} aria-label="done" />;
    case 'skipped':
      return <MinusCircle className={`${size} text-slate-400`} aria-label="skipped" />;
    case 'failed':
      return <XCircle className={`${size} text-rose-600`} aria-label="failed" />;
    case 'cancelled':
      return <PauseCircle className={`${size} text-amber-600`} aria-label="cancelled" />;
    default:
      return <CircleDashed className={`${size} text-slate-300`} aria-label="pending" />;
  }
}

export function ErrorNote({ error }: { error: unknown }) {
  if (!error) return null;
  return (
    <p role="alert" className="rounded-md bg-rose-50 px-3 py-2 text-sm text-rose-800 ring-1 ring-rose-200">
      {error instanceof Error ? error.message : String(error)}
    </p>
  );
}

export function Details({ title, children }: { title: string; children: string }) {
  return (
    <details>
      <summary className="cursor-pointer text-slate-700">{title}</summary>
      <pre className="mt-1 max-h-64 overflow-auto rounded bg-slate-50 p-2 font-mono text-xs">{children}</pre>
    </details>
  );
}

