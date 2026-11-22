import { useEffect, useRef, useState } from 'react';
import { followRun, Waker } from './api';
import type { LogLine } from './types';
import { LogModel } from './logmodel';

/** Lines kept in memory; older ones are dropped (the server keeps all). */
const MAX_LINES = 300000;
/** Dropped at once when the limit is reached, so the model is rebuilt rarely. */
const DROP_LINES = 50000;

export interface Log {
  lines: LogLine[];
  model: LogModel;
  version: number;
  connection: 'connecting' | 'connected' | 'reconnecting' | 'idle';
  filter: string;
  setFilter: (step: string) => void;
  /** Reconnect a stream that ended because the run had settled. */
  wake: () => void;
}

export function useLog(id: string, onRun: () => void): Log {
  const lines = useRef<LogLine[]>([]);
  const model = useRef(new LogModel());
  const lastSeq = useRef(0);
  const waker = useRef(new Waker());
  const [version, setVersion] = useState(0);
  const [connection, setConnection] = useState<Log['connection']>('connecting');
  const [filter, setFilter] = useState('all');
  const onRunRef = useRef(onRun);
  onRunRef.current = onRun;

  useEffect(() => {
    lines.current = [];
    model.current = new LogModel();
    lastSeq.current = 0;
    setVersion((v) => v + 1);
    const abort = new AbortController();
    let pending = false;
    const flush = () => {
      pending = false;
      setVersion((v) => v + 1);
    };
    void followRun(
      id,
      () => lastSeq.current,
      {
        onLog: (line) => {
          if (line.seq <= lastSeq.current) return;
          lastSeq.current = line.seq;
          lines.current.push(line);
          if (lines.current.length > MAX_LINES + DROP_LINES) {
            // Control notes stay: they number the release's attempts and say who started them.
            const control = lines.current.slice(0, DROP_LINES).filter((l) => l.step === 'control');
            lines.current = [...control, ...lines.current.slice(DROP_LINES)];
            model.current = new LogModel();
            for (const kept of lines.current) model.current.append(kept);
          } else {
            model.current.append(line);
          }
          // Re-render a few times per second however fast lines arrive; less often
          // for big logs, where a render costs more.
          if (!pending) {
            pending = true;
            setTimeout(flush, lines.current.length > 50000 ? 500 : 150);
          }
        },
        onRun: () => onRunRef.current(),
        onState: setConnection,
      },
      abort.signal,
      waker.current,
    );
    return () => abort.abort();
  }, [id]);

  return {
    lines: lines.current,
    model: model.current,
    version,
    connection,
    filter,
    setFilter,
    wake: waker.current.wake,
  };
}
