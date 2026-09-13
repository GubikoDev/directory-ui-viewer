import { describe, expect, it } from 'vitest';
import { ReplyInbox } from './inbox';

describe('native response inbox', () => {
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
