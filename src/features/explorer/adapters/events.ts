import type { WorkEvent } from '../model/types';
import { frontendBudget, type FrontendBudget } from '../runtime/memory';

function validEvent(value: unknown): value is WorkEvent {
  if (!value || typeof value !== 'object') return false;
  const event = value as Partial<WorkEvent>;
  return (
    event.protocolVersion === 1 &&
    typeof event.sessionId === 'string' &&
    event.sessionId.length <= 256 &&
    typeof event.taskId === 'string' &&
    event.taskId.length <= 256 &&
    Number.isSafeInteger(event.generation) &&
    (event.generation ?? 0) > 0 &&
    Number.isSafeInteger(event.sequence) &&
    (event.sequence ?? -1) >= 0 &&
    typeof event.observedAt === 'string' &&
    event.observedAt.length <= 64 &&
    (event.kind === 'listingAvailable' ||
      event.kind === 'usageChanged' ||
      event.kind === 'progress' ||
      event.kind === 'terminal')
  );
}

/** Hints are best-effort. Only one document subscription and no JS event queue. */
export class WorkInbox {
  private listener?: (event: WorkEvent) => void;
  private subscription = 0;
  constructor(private readonly budget: FrontendBudget = frontendBudget) {
    budget.retainLifetime(this, 2048);
  }
  subscribe(listener: (event: WorkEvent) => void): () => void {
    if (this.listener || !Number.isSafeInteger(this.subscription + 1))
      throw new Error('RESOURCE_LIMIT');
    const subscription = ++this.subscription;
    this.listener = listener;
    return () => {
      if (this.subscription === subscription) this.listener = undefined;
    };
  }
  receive = (json: string): boolean => {
    if (!this.listener || typeof json !== 'string' || json.length > 4096)
      return false;
    try {
      // parse reserves encoded + decoded costs before allocation. Retained
      // event objects stay charged even after delivery/subscription ends.
      const event = this.budget.parse(json);
      if (!validEvent(event)) return false;
      this.listener(event);
      return true;
    } catch {
      // A dropped hint is recovered by readTask polling, never by an unbounded retry queue.
      return false;
    }
  };
}
let inbox: WorkInbox | undefined;
export function nativeEvents(): WorkInbox {
  const target = window as Window & {
    __DIRECTORY_WORK_EVENT__?: WorkInbox['receive'];
  };
  inbox ??= new WorkInbox();
  if (!target.__DIRECTORY_WORK_EVENT__)
    Object.defineProperty(target, '__DIRECTORY_WORK_EVENT__', {
      value: inbox.receive,
    });
  if (target.__DIRECTORY_WORK_EVENT__ !== inbox.receive)
    throw new Error('CLIENT_EXPIRED');
  return inbox;
}
