export type RecordedTime = { kind: 'known'; at: string } | { kind: 'unknown' };

export type RunState =
  | { kind: 'queued' }
  | { kind: 'running' }
  | { kind: 'retry_wait'; due_at: string | null; error: string }
  | { kind: 'failing'; error: string }
  | { kind: 'succeeded'; completed_at: RecordedTime }
  | { kind: 'failed'; error: string; completed_at: RecordedTime }
  | { kind: 'cancelling'; reason: string | null }
  | { kind: 'cancelled'; reason: string | null; completed_at: RecordedTime }
  | { kind: 'blocked'; error: string }
  | { kind: 'compensating' }
  | { kind: 'compensation_failed'; error: string }
  | { kind: 'compensated'; completed_at: RecordedTime };

export type StepState =
  | { kind: 'pending' }
  | { kind: 'running'; started_at: RecordedTime }
  | { kind: 'retry_wait'; error: string }
  | { kind: 'succeeded'; completed_at: RecordedTime }
  | { kind: 'failed'; error: string; completed_at: RecordedTime }
  | { kind: 'cancelled'; completed_at: RecordedTime };

export interface StepProgress {
  id: string;
  stage: number;
  state: StepState;
  attempts: number;
}

export interface RunProgress {
  id: string;
  definition: string;
  epoch: number;
  cursor: number;
  state: RunState;
  created_at: string;
  updated_at: string;
  steps: StepProgress[];
}

export type AttemptOutcome =
  | { kind: 'succeeded' }
  | { kind: 'failed'; error: string }
  | { kind: 'cancelled' }
  | { kind: 'interrupted'; error: string };

export type AttemptState =
  | { kind: 'running'; started_at: string }
  | { kind: 'finished'; started_at: string; finished_at: string; outcome: AttemptOutcome }
  | { kind: 'incomplete'; started_at: string; outcome: AttemptOutcome };

export interface StepHistory {
  id: string;
  attempts: { number: number; epoch: number; state: AttemptState }[];
}

export interface RunHistory {
  steps: StepHistory[];
}

export interface Package {
  package_name: string;
  version: string;
}

export interface Release extends Package {
  tag: string;
}

export interface ReleaseFacts {
  dry_run?: boolean;
  main_sha?: string | null;
  pr?: { number: number; url: string } | null;
  packages?: Package[];
  nothing_to_release?: boolean;
  update_diff?: string | null;
  pre_released?: Release[] | null;
  merge_sha?: string | null;
  publish?: { attempt: number; releases: Release[]; note: string | null } | null;
  releases?: Release[] | null;
  verify?: {
    checked: number;
    missing_on_crates_io: string[];
    missing_tags: string[];
    errors: string[];
    /** Package name → the crates.io version a release replaced. */
    previous?: Record<string, string>;
    /** Packages released as tags only, not on crates.io. */
    git_only?: string[];
  } | null;
  skipped?: string[];
}

export interface RunRecord {
  run_id: string;
  started_by: string;
  dry_run: boolean;
  created_at: string;
  facts: ReleaseFacts;
}

export interface RunSummary {
  progress: RunProgress;
  record: RunRecord | null;
}

export interface RunDetail extends RunSummary {
  history: RunHistory;
}

export interface LogLine {
  seq: number;
  run_id: string;
  step: string;
  attempt: number;
  stream: 'stdout' | 'stderr' | 'cmd' | 'info';
  line: string;
  ts: string;
}

export interface Me {
  uid: string;
  email: string;
  steps: string[];
}
