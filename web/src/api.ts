import { token } from './firebase';
import { SseParser } from './sse';
import type { LogLine, RunProgress } from './types';

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public code?: string,
  ) {
    super(message);
  }
}

const BASE = '/publisher/v1';

async function authorized(force: boolean): Promise<string> {
  const jwt = await token(force);
  if (!jwt) throw new ApiError(401, 'Sign in with Google');
  return jwt;
}

export async function api<T>(path: string, method = 'GET', body?: unknown, force = false): Promise<T> {
  const jwt = await authorized(force);
  let response: Response;
  try {
    response = await fetch(BASE + path, {
      method,
      headers: {
        Authorization: `Bearer ${jwt}`,
        ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: 'no-store',
      signal: AbortSignal.timeout(30000),
    });
  } catch {
    throw new ApiError(0, 'The server is not reachable');
  }
  // An expired ID token: refresh once.
  if (response.status === 401 && !force) return api<T>(path, method, body, true);
  if (!response.ok) {
    const problem = await response.json().catch(() => ({}));
    throw new ApiError(
      response.status,
      problem.detail || problem.title || `Request failed (${response.status})`,
      problem.code,
    );
  }
  return response.json() as Promise<T>;
}

export interface StreamHandlers {
  onLog: (line: LogLine) => void;
  onRun: (progress: RunProgress) => void;
  onState: (state: 'connected' | 'reconnecting' | 'idle') => void;
}

/** Resolves on `wake()`, after `ms`, or when `signal` aborts. */
export class Waker {
  private resolve: (() => void) | null = null;
  wake = () => this.resolve?.();
  wait(ms: number, signal: AbortSignal): Promise<void> {
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        signal.removeEventListener('abort', done);
        this.resolve = null;
        resolve();
      };
      const timer = setTimeout(done, ms);
      signal.addEventListener('abort', done);
      this.resolve = done;
    });
  }
}

/** How often a settled run is checked again in case someone else continued it. */
const IDLE_RECHECK = 60000;

/** The server's maximum page size for `/runs/{id}/log`. */
const LOG_PAGE = 5000;

/**
 * Follows `/runs/{id}/stream` until `signal` aborts, reconnecting with the last seen
 * `seq`. EventSource cannot send `Authorization`, so this reads the stream with fetch.
 * The server ends the stream with `end` once the run is settled; the client then idles
 * until `waker` is woken (Retry, Resume) or a minute has passed. Every reconnect sends
 * a fresh ID token.
 */
export async function followRun(
  id: string,
  after: () => number,
  handlers: StreamHandlers,
  signal: AbortSignal,
  waker: Waker,
): Promise<void> {
  let delay = 1000;
  let force = false;
  while (!signal.aborted) {
    let ended = false;
    try {
      // The backlog comes from the paged endpoint, which is gzipped (SSE is not, and
      // its per-line JSON is ~20x the compressed size); the stream then sends the tail.
      for (;;) {
        const page = await api<LogLine[]>(`/runs/${id}/log?after=${after()}&limit=${LOG_PAGE}`);
        if (signal.aborted) return;
        for (const line of page) handlers.onLog(line);
        if (page.length < LOG_PAGE) break;
      }
      const jwt = await authorized(force);
      const response = await fetch(`${BASE}/runs/${id}/stream?after=${after()}`, {
        headers: { Authorization: `Bearer ${jwt}`, Accept: 'text/event-stream' },
        cache: 'no-store',
        signal,
      });
      if (response.status === 401 && !force) {
        force = true;
        continue;
      }
      if (!response.ok || !response.body) throw new ApiError(response.status, 'stream failed');
      force = false;
      delay = 1000;
      handlers.onState('connected');
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      const parser = new SseParser();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        for (const event of parser.push(value)) {
          if (event.event === 'log') handlers.onLog(JSON.parse(event.data) as LogLine);
          else if (event.event === 'run') handlers.onRun(JSON.parse(event.data) as RunProgress);
          else if (event.event === 'end') ended = true;
        }
      }
      if (ended) {
        handlers.onState('idle');
        await waker.wait(IDLE_RECHECK, signal);
        continue;
      }
      // A normal close (the server's lifetime limit): reconnect at once.
      continue;
    } catch (error) {
      if (signal.aborted) return;
      if (error instanceof ApiError && error.status === 404) return;
    }
    if (signal.aborted) return;
    handlers.onState('reconnecting');
    await waker.wait(delay, signal);
    delay = Math.min(delay * 2, 15000);
  }
}

/** The whole log of a run as text, read page by page. */
export async function fullLog(id: string): Promise<string> {
  const out: string[] = [];
  let after = 0;
  for (;;) {
    const page = await api<LogLine[]>(`/runs/${id}/log?after=${after}&limit=${LOG_PAGE}`);
    for (const l of page) out.push(`${l.ts} ${l.step}#${l.attempt} ${l.stream.padEnd(6)} ${l.line}`);
    if (page.length < LOG_PAGE) return out.join('\n') + '\n';
    after = page[page.length - 1].seq;
  }
}
