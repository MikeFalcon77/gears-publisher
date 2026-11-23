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

export interface RunRecord<F = ReleaseFacts> {
  run_id: string;
  started_by: string;
  dry_run: boolean;
  created_at: string;
  facts: F;
}

export type RunKind = 'release' | 'ci';

export interface RunSummary<F = ReleaseFacts> {
  /** `null` for a workflow this UI does not know. */
  kind?: RunKind | null;
  progress: RunProgress;
  record: RunRecord<F> | null;
}

export interface RunDetail<F = ReleaseFacts> extends RunSummary<F> {
  history: RunHistory;
}

export type JobConclusion = 'success' | 'failure' | 'skipped';

export interface JobResult {
  id: string;
  conclusion: JobConclusion;
  failed_step?: string | null;
  reason?: string | null;
  /** Why it failed when the host broke it (disk, memory), not the code. */
  host_problem?: string | null;
  /** Failing tests and crates that did not compile. */
  failures?: string[];
}

/** PR CI's state (ci/mod.rs `CiState`); the first facts hold only `pr_number`/`only`. */
export interface CiFacts {
  pr_number?: number;
  pr?: {
    number: number;
    title: string;
    url: string;
    author: string;
    head_ref: string;
    head_sha: string;
    base_ref: string;
    cross_repository: boolean;
  } | null;
  only?: string[] | null;
  merge_sha?: string | null;
  base_sha?: string | null;
  changed_files?: number;
  files?: string[];
  changes?: { filters: Record<string, boolean>; ci_workflow: boolean; fmt_workflow: boolean } | null;
  rules?: string | null;
  jobs?: JobResult[];
  skipped?: string[];
}

export type CiRun = RunDetail<CiFacts>;
export type CiSummary = RunSummary<CiFacts>;

export interface CiJob {
  id: string;
  name: string;
  workflow: string;
  steps: string[];
  note: string | null;
}

export interface CiInfo {
  steps: string[];
  jobs: CiJob[];
  repository: string;
  /** Why PR CI cannot run on this host. */
  unavailable: string | null;
}

/** `POST /ci`: a started run, or a queue entry when another PR's CI is running. */
export interface Started {
  /** The run; the queue entry when `queued`. */
  id: string;
  queued: boolean;
  /** Place in the queue, 1: next. */
  position?: number;
}

/** A PR CI waiting for the run in progress. A member sees only their own. */
export interface QueuedCi {
  id: string;
  /** Place among everyone's entries, 1: next. */
  position: number;
  pr: number;
  jobs: string[] | null;
  requested_by: string;
  queued_at: string;
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

/** `none`: signed in, never asked for access. */
export type Role = 'admin' | 'member' | 'pending' | 'rejected' | 'none';

export interface Me {
  uid: string;
  email: string | null;
  name: string | null;
  role: Role;
  /** The note of a pending or rejected request. */
  note: string | null;
  /** Requests waiting for a decision; admins only. */
  pending_requests: number | null;
  /** Release steps. */
  steps: string[];
  ci: CiInfo;
}

export type AccessStatus = 'pending' | 'approved' | 'rejected';

export interface UserEntry {
  subject_id: string;
  uid: string;
  email: string | null;
  name: string | null;
  /** `google.com`, `github.com`, ... */
  provider: string | null;
  note: string;
  status: AccessStatus;
  requested_at: string;
  decided_at: string | null;
  decided_by: string | null;
}

export interface AccessList {
  admins: { uid: string; email: string | null }[];
  users: UserEntry[];
}
