import { describe, expect, it } from 'vitest';
import { SseParser } from './sse';

describe('SseParser', () => {
  it('assembles events split across chunks', () => {
    const parser = new SseParser();
    expect(parser.push('event: log\nid: 7\nda')).toEqual([]);
    expect(parser.push('ta: {"seq":7}\n\n: keep-alive\n\nevent: run\ndata: {}\n\n')).toEqual([
      { event: 'log', id: '7', data: '{"seq":7}' },
      { event: 'run', id: undefined, data: '{}' },
    ]);
  });

  it('joins multi-line data and handles CRLF', () => {
    const parser = new SseParser();
    expect(parser.push('data: a\r\ndata: b\r\n\r\n')).toEqual([{ event: 'message', data: 'a\nb', id: undefined }]);
  });
});
