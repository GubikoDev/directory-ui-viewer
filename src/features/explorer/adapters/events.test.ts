import { describe, expect, it, vi } from 'vitest';
import { FrontendBudget, ownBytes } from '../runtime/memory';
import wire from '../model/wire-fixture.json';
import type { WorkEvent } from '../model/types';
import { WorkInbox } from './events';

describe('bounded native event delivery', () => {
  it('delivers typed hints once per call and an old unsubscribe cannot remove a new subscription', () => {
    const inbox = new WorkInbox(new FrontendBudget());
    const listener = vi.fn();
    const json = JSON.stringify(wire.event);
    expect(inbox.receive(json)).toBe(false);
    const old = inbox.subscribe(listener);
    expect(() => inbox.subscribe(listener)).toThrow('RESOURCE_LIMIT');
    expect(inbox.receive(json)).toBe(true);
    expect(listener).toHaveBeenCalledWith(wire.event);
    old();
    expect(inbox.receive(json)).toBe(false);
    const current = inbox.subscribe(listener);
    old();
    expect(inbox.receive(json)).toBe(true);
    current();
    expect(inbox.receive(json)).toBe(false);
  });
  it('drops malformed, oversized and memory-pressure hints without a retry queue', () => {
    const budget = new FrontendBudget();
    const inbox = new WorkInbox(budget);
    const listener = vi.fn();
    inbox.subscribe(listener);
    for (const json of [
      'null',
      'bad',
      'x'.repeat(4097),
      JSON.stringify({ ...wire.event, sequence: -1 }),
    ])
      expect(inbox.receive(json)).toBe(false);
    expect(listener).not.toHaveBeenCalled();
    const blocker = budget.reserve(budget.limit - budget.used);
    expect(inbox.receive(JSON.stringify(wire.event))).toBe(false);
    expect(listener).not.toHaveBeenCalled();
    blocker.close();
    expect(inbox.receive(JSON.stringify(wire.event))).toBe(true);
    expect(listener).toHaveBeenCalledOnce();
  });
  it('retained decoded events remain charged after unsubscribe', () => {
    const records = new Map<object, number>();
    let collect!: (bytes: number) => void;
    const budget = new FrontendBudget(64 * 1024, (release) => {
      collect = release;
      return {
        register: (owner, bytes) => {
          records.set(owner, bytes);
        },
      };
    });
    const inbox = new WorkInbox(budget);
    const baseline = budget.used;
    let event!: WorkEvent;
    const stop = inbox.subscribe((value) => {
      event = value;
    });
    expect(inbox.receive(JSON.stringify(wire.event))).toBe(true);
    stop();
    expect(budget.used).toBe(baseline + 1024 + ownBytes(event));
    expect(Object.isFrozen(event)).toBe(true);
    collect(records.get(event)!);
    expect(budget.used).toBe(baseline + 1024);
    collect(records.get(stop)!);
    expect(budget.used).toBe(baseline);
  });
});
