import { describe, expect, it, vi } from 'vitest';
import { FrontendBudget } from './memory';
import { TaskMap } from './task-map';

function accounting(limit = 64 * 1024) {
  const records = new Map<object, number>();
  let release!: (bytes: number) => void;
  const budget = new FrontendBudget(limit, (collect) => {
    release = collect;
    return {
      register(owner, bytes) {
        records.set(owner, (records.get(owner) ?? 0) + bytes);
      },
    };
  });
  return {
    budget,
    records,
    collect(owner: object) {
      release(records.get(owner)!);
      records.delete(owner);
    },
  };
}
describe('client task metadata ownership', () => {
  it('deletes under full pressure and later compacts capacity without releasing the old owner early', () => {
    const { budget, records, collect } = accounting();
    const tasks = new TaskMap<number>(budget);
    tasks.set('keep', 1);
    tasks.set('closed', 2);
    const old = [...records.keys()].find((value) => value instanceof Map)!;
    const pressure = budget.reserve(budget.available);
    tasks.prune((key) => key === 'keep');
    expect(tasks.size).toBe(1);
    expect(budget.used).toBe(budget.limit);
    pressure.close();
    tasks.prune((key) => key === 'keep');
    expect(records.has(old)).toBe(true);
    const current = [...records.keys()].find(
      (value) => value instanceof Map && value !== old,
    )!;
    expect(current).toBeDefined();
    collect(old);
    expect(tasks.get('keep')).toBe(1);
    tasks.clear();
    collect(current);
    expect(budget.used).toBe(512);
  });
  it('precharges growth and keeps deleted map capacity until that container is collected', () => {
    const { budget, records, collect } = accounting();
    const tasks = new TaskMap<number>(budget);
    const baseline = budget.used;
    tasks.set('one', 1);
    tasks.set('two', 2);
    const container = [...records.keys()].find(
      (value) => value instanceof Map,
    )!;
    const used = budget.used;
    tasks.set('one', 3);
    expect(budget.used).toBe(used);
    const blocker = budget.reserve(budget.limit - budget.used);
    expect(() => tasks.set('three', 4)).toThrow('RESOURCE_LIMIT');
    tasks.delete('one');
    tasks.clear();
    expect(tasks.size).toBe(0);
    expect(budget.used).toBe(budget.limit);
    blocker.close();
    collect(container);
    expect(budget.used).toBe(baseline);
  });
  it('does not start async work when its map or promise cannot be admitted', () => {
    const { budget } = accounting(1024);
    const tasks = new TaskMap<Promise<void>>(budget);
    const start = vi.fn(async () => undefined);
    expect(() => tasks.run('one', start)).toThrow('RESOURCE_LIMIT');
    expect(start).not.toHaveBeenCalled();
    expect(tasks.size).toBe(0);
    expect(budget.used).toBeLessThanOrEqual(budget.limit);
  });
  it('retains pending promise cost after clear and an old completion cannot delete its replacement', async () => {
    const { budget, records, collect } = accounting();
    const tasks = new TaskMap<Promise<void>>(budget);
    let finishOld!: () => void;
    let finishNew!: () => void;
    const old = tasks.run(
      'same',
      () =>
        new Promise<void>((resolve) => {
          finishOld = resolve;
        }),
    );
    expect(tasks.run('same', async () => undefined)).toBe(old);
    const oldMap = [...records.keys()].find((value) => value instanceof Map)!;
    tasks.clear();
    collect(oldMap);
    expect(records.get(old)).toBe(4096 + 'same'.length * 2);
    const current = tasks.run(
      'same',
      () =>
        new Promise<void>((resolve) => {
          finishNew = resolve;
        }),
    );
    finishOld();
    await old;
    expect(tasks.get('same')).toBe(current);
    collect(old);
    finishNew();
    await current;
    expect(tasks.size).toBe(0);
    expect(records.has(current)).toBe(true);
  });
});
