import { describe, expect, it, vi } from 'vitest';
import { FrontendBudget, ownBytes } from './memory';

function harness(limit = 32 * 1024 * 1024) {
  const records = new Map<object, number[]>();
  let release!: (bytes: number) => void;
  const budget = new FrontendBudget(limit, (callback) => {
    release = callback;
    return {
      register(target, bytes) {
        records.set(target, [...(records.get(target) ?? []), bytes]);
      },
    };
  });
  return {
    budget,
    collect(target: object) {
      for (const bytes of records.get(target) ?? []) release(bytes);
      records.delete(target);
    },
  };
}
describe('document-wide managed observation budget', () => {
  it('moves unused capacity only within the same pool and admission class', () => {
    const budget = new FrontendBudget(100, undefined, 20);
    const source = budget.reserve(50);
    const target = budget.reserve(30);
    const control = budget.reserve(20, true);
    source.moveTo(target, 40);
    expect([source.bytes, target.bytes, budget.used]).toEqual([10, 70, 100]);
    expect(() => source.moveTo(target, 11)).toThrow('RESOURCE_LIMIT');
    expect(() => source.moveTo(target, -1)).toThrow('RESOURCE_LIMIT');
    expect(() => source.moveTo(new FrontendBudget().reserve(0), 1)).toThrow(
      'INVALID_RESERVATION',
    );
    expect(() => control.moveTo(target, 1)).toThrow('INVALID_RESERVATION');
    source.close();
    target.close();
    control.close();
    expect(budget.used).toBe(0);
  });
  it('transfers preadmitted parse credit without freeing it or crossing pools', () => {
    const { budget, collect } = harness();
    const json = '{"sessionId":"owned"}';
    const size = budget.parseBytes(json);
    const parent = budget.reserve(size + 100);
    const child = parent.take(size);
    expect(budget.used).toBe(size + 100);
    expect(parent.bytes).toBe(100);
    expect(() => parent.take(101)).toThrow('RESOURCE_LIMIT');
    expect(() => new FrontendBudget().parse(json, false, child)).toThrow(
      'INVALID_RESERVATION',
    );
    const result = budget.parse(json, false, child) as object;
    parent.close();
    child.close();
    expect(budget.used).toBe(ownBytes(result));
    collect(result);
    expect(budget.used).toBe(0);
  });
  it('keeps control reservations in the same counter and absolute ceiling', () => {
    const budget = new FrontendBudget(100, undefined, 20);
    const data = budget.reserve(80);
    expect(() => budget.reserve(1)).toThrow('RESOURCE_LIMIT');
    const control = budget.reserve(10, true);
    control.resize(20);
    expect(budget.used).toBe(100);
    expect(budget.available).toBe(0);
    expect(() => control.resize(21)).toThrow('RESOURCE_LIMIT');
    data.resize(70);
    expect(() => data.resize(71)).toThrow('RESOURCE_LIMIT');
    control.close();
    expect(budget.available).toBe(10);
    data.close();
    expect(budget.used).toBe(0);
    expect(budget.peak).toBe(100);
  });
  it('checks the entire new graph before transferring any reservation to finalizers', () => {
    const { budget } = harness();
    const value = { child: { name: 'owned' }, sibling: { count: 10 } };
    const required = budget.untrackedBytes(value);
    expect(() => budget.allocate(required - 1, () => value)).toThrow(
      'RESOURCE_LIMIT',
    );
    expect(budget.used).toBe(0);
    expect(budget.untrackedBytes(value)).toBe(required);
    const other = new FrontendBudget();
    const memory = other.reserve(required);
    expect(() => budget.retain(value, memory)).toThrow('INVALID_RESERVATION');
    expect(budget.used).toBe(0);
    memory.close();
    expect(other.used).toBe(0);
  });
  it('rejects allocation before entering a factory and preserves existing reservations', () => {
    const budget = new FrontendBudget(1000);
    const held = budget.reserve(800);
    const factory = vi.fn(() => ({}));
    expect(() => budget.allocate(201, factory)).toThrow('RESOURCE_LIMIT');
    expect(factory).not.toHaveBeenCalled();
    expect(() => held.resize(1001)).toThrow('RESOURCE_LIMIT');
    expect(held.bytes).toBe(800);
    expect(budget.used).toBe(800);
    held.close();
    held.close();
    expect(budget.used).toBe(0);
    expect(() =>
      budget.allocate(100, () => {
        throw new Error('failure');
      }),
    ).toThrow('failure');
    expect(budget.used).toBe(0);
    expect(budget.peak).toBe(800);
  });
  it('keeps shared children charged after the response container is collected', () => {
    const { budget, collect } = harness();
    const child = { name: 'retained 한글😀', bytes: '9007199254740993' };
    const root = budget.adopt({ first: child, second: child });
    expect(budget.used).toBe(ownBytes(root) + ownBytes(child));
    const held = budget.used;
    budget.adopt(child);
    expect(budget.used).toBe(held);
    collect(root);
    expect(budget.used).toBe(ownBytes(child));
    collect(child);
    expect(budget.used).toBe(0);
  });
  it('accounts independent map snapshots until each is collected', () => {
    const { budget, collect } = harness();
    const entry = { displayName: 'owned' };
    const first = budget.adopt(new Map([['entry', entry]]));
    const second = budget.allocate(ownBytes(first), () => new Map(first));
    expect(budget.used).toBe(
      ownBytes(first) + ownBytes(second) + ownBytes(entry),
    );
    collect(first);
    expect(budget.used).toBe(ownBytes(second) + ownBytes(entry));
    collect(second);
    expect(budget.used).toBe(ownBytes(entry));
    collect(entry);
    expect(budget.used).toBe(0);
  });
  it('reserves parsed graphs before JSON.parse, including escaping and Unicode', () => {
    const json = JSON.stringify({
      rows: Array.from({ length: 200 }, (_, n) => ({
        id: n,
        text: ('한글😀' + '\\' + '"{[,:]}\n').repeat(10),
        value: false,
      })),
    });
    const { budget } = harness();
    const parsed = budget.parse(json) as { rows: unknown[] };
    expect(parsed.rows).toHaveLength(200);
    expect(Object.isFrozen(parsed)).toBe(true);
    expect(Object.isFrozen(parsed.rows)).toBe(true);
    expect(Object.isFrozen(parsed.rows[0])).toBe(true);
    expect(budget.used).toBeGreaterThan(json.length);
    expect(budget.used).toBeLessThanOrEqual(budget.peak);
    const parse = vi.spyOn(JSON, 'parse');
    try {
      expect(() => new FrontendBudget(1).parse(json)).toThrow('RESOURCE_LIMIT');
      expect(parse).not.toHaveBeenCalled();
      expect(() => budget.parse('['.repeat(65) + ']'.repeat(65))).toThrow(
        'INVALID_RESPONSE',
      );
      expect(parse).not.toHaveBeenCalled();
    } finally {
      parse.mockRestore();
    }
  });
  it('rejects cyclic adoption and malformed wire without silently consuming the pool', () => {
    const { budget } = harness();
    const cycle: { self?: unknown } = {};
    cycle.self = cycle;
    expect(() => budget.adopt(cycle)).toThrow('INVALID_ARGUMENT');
    expect(() => budget.adopt(new Date())).toThrow('INVALID_ARGUMENT');
    expect(() => budget.parse('{')).toThrow();
    expect(() => budget.parse('null')).toThrow('INVALID_RESPONSE');
    expect(budget.used).toBe(0);
  });
});
