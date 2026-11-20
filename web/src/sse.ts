/** One Server-Sent Event. */
export interface SseEvent {
  event: string;
  data: string;
  id?: string;
}

/**
 * Incremental `text/event-stream` parser: feed decoded chunks, get complete events.
 * Comments (`:`, used for keep-alive) are dropped.
 */
export class SseParser {
  private buffer = '';

  push(chunk: string): SseEvent[] {
    this.buffer += chunk.replace(/\r\n?/g, '\n');
    const events: SseEvent[] = [];
    let end: number;
    while ((end = this.buffer.indexOf('\n\n')) >= 0) {
      const block = this.buffer.slice(0, end);
      this.buffer = this.buffer.slice(end + 2);
      const event = parseBlock(block);
      if (event) events.push(event);
    }
    return events;
  }
}

function parseBlock(block: string): SseEvent | null {
  let event = 'message';
  let id: string | undefined;
  const data: string[] = [];
  for (const line of block.split('\n')) {
    if (!line || line.startsWith(':')) continue;
    const colon = line.indexOf(':');
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') event = value;
    else if (field === 'data') data.push(value);
    else if (field === 'id') id = value;
  }
  return data.length ? { event, data: data.join('\n'), id } : null;
}
