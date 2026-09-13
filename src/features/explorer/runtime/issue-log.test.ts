import { describe, expect, it, vi } from 'vitest';
import { FrontendBudget } from './memory';
import { IssueLog } from './issue-log';

describe('bounded diagnostic observations', () => {
  it('keeps 32 immutable codes without retaining Error causes or calling message getters', () => {
    const log = new IssueLog(new FrontendBudget());
    for (let i = 0; i < 35; i++) log.append(new Error(`CODE_${i}`));
    expect(log.snapshot).toHaveLength(32);
    expect(log.snapshot[0].message).toBe('CODE_3');
    expect(Object.isFrozen(log.snapshot)).toBe(true);
    log.append(
      new Error('/private/filename', { cause: { large: 'x'.repeat(100000) } }),
    );
    expect(log.snapshot.at(-1)).toEqual({ message: 'INTERNAL' });
    const getter = vi.fn(() => 'SECRET');
    log.append(Object.defineProperty(new Error(), 'message', { get: getter }));
    expect(getter).not.toHaveBeenCalled();
    expect(log.snapshot.at(-1)).toEqual({ message: 'INTERNAL' });
  });
  it('reports and clears under full pressure without allocating new observations', () => {
    const budget = new FrontendBudget();
    const log = new IssueLog(budget);
    const empty = log.snapshot;
    const pressure = budget.reserve(budget.available);
    log.append(new Error('IO_ERROR'));
    const snapshot = log.snapshot;
    expect(snapshot).toEqual([{ message: 'RESOURCE_LIMIT' }]);
    const used = budget.used;
    for (let i = 0; i < 100; i++) log.append(new Error('IO_ERROR'));
    expect(log.snapshot).toBe(snapshot);
    expect(budget.used).toBe(used);
    log.clear();
    expect(log.snapshot).toBe(empty);
    pressure.close();
  });
});
