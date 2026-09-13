import { describe, expect, it } from 'vitest';
import { ReplyInbox } from './inbox';
import { FrontendBudget, ownBytes } from '../runtime/memory';

describe('native response inbox', () => {
  it('protects admitted mutation bytes from later inbox saturation', () => {
    const inbox = new ReplyInbox(32 * 1024, 3, new FrontendBudget());
    inbox.open('mutation', false, 16 * 1024);
    inbox.open('later', false, 16 * 1024);
    expect(inbox.bytes).toBe(32 * 1024);
    expect(() => inbox.open('extra')).toThrow('RESOURCE_LIMIT');
    expect(inbox.receive('mutation', '{"sessionId":"owned"}')).toBe(true);
    expect(inbox.parse('mutation')).toEqual({ sessionId: 'owned' });
    inbox.close('mutation');
    inbox.close('later');
    expect(inbox.bytes).toBe(0);
  });
  it('keeps control slot and byte headroom without evicting ordinary waiters', () => {
    const inbox = new ReplyInbox(2048, 3, new FrontendBudget(), 1, 1024);
    inbox.open('one');
    inbox.open('two');
    expect(() => inbox.open('ordinary')).toThrow('RESOURCE_LIMIT');
    inbox.open('control', true);
    expect(inbox.receive('control', '{"closed":true}')).toBe(true);
    expect(inbox.parse('control')).toEqual({ closed: true });
    expect(inbox.pending).toBe(3);
    expect(() => inbox.open('extra', true)).toThrow('RESOURCE_LIMIT');
    for (const key of ['one', 'two', 'control']) inbox.close(key);
    expect(inbox.bytes).toBe(0);
  });
  it('shares admission across inboxes and retains parsed results after closing a slot', () => {
    let collect!: (bytes: number) => void;
    const records = new Map<object, number>();
    const budget = new FrontendBudget(64 * 1024, (callback) => {
      collect = callback;
      return {
        register(target, bytes) {
          records.set(target, bytes);
        },
      };
    });
    const first = new ReplyInbox(8192, 2, budget);
    const second = new ReplyInbox(8192, 2, budget);
    const baseline = budget.used;
    first.open('one');
    second.open('two');
    first.receive('one', '{"ok":true}');
    const result = first.parse('one') as object;
    first.close('one');
    expect(budget.used).toBe(baseline + 512 + ownBytes(result));
    second.close('two');
    expect(budget.used).toBe(baseline + ownBytes(result));
    collect(records.get(result)!);
    expect(budget.used).toBe(baseline);
    const reservation = budget.reserve(budget.available - 512);
    first.open('three');
    expect(() => second.open('four')).toThrow('RESOURCE_LIMIT');
    expect(second.pending).toBe(0);
    first.close('three');
    reservation.close();
    expect(budget.used).toBe(baseline);
  });
  it('retains encoded replies until the IPC completion ack is handled', () => {
    const inbox = new ReplyInbox();
    inbox.open('one');
    expect(inbox.bytes).toBe(512);
    expect(inbox.receive('one', '{"ok":true}')).toBe(true);
    expect(inbox.bytes).toBe(512 + 22);
    expect(inbox.read('one')).toBe('{"ok":true}');
    expect(inbox.receive('one', 'null')).toBe(false);
    inbox.close('one');
    expect(inbox.bytes).toBe(0);
    expect(inbox.pending).toBe(0);
    expect(inbox.receive('one', 'null')).toBe(false);
  });
  it('bounds pending slots and encoded payloads without evicting a live waiter', () => {
    const inbox = new ReplyInbox(1100, 2);
    inbox.open('one');
    inbox.open('two');
    expect(() => inbox.open('three')).toThrow('RESOURCE_LIMIT');
    expect(inbox.receive('one', 'x'.repeat(100))).toBe(true);
    expect(() => inbox.read('one')).toThrow('RESOURCE_LIMIT');
    expect(inbox.bytes).toBe(1024);
    inbox.close('one');
    expect(inbox.receive('two', 'null')).toBe(true);
    expect(inbox.read('two')).toBe('null');
    inbox.close('two');
    expect(inbox.bytes).toBe(0);
  });
});
