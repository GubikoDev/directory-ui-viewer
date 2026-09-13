import { describe, expect, it, vi } from 'vitest';
import { FrontendBudget } from '../runtime/memory';
import { encodeWire } from './encoding';

describe('bounded JSON wire encoding', () => {
  it('admits buffer and view together before allocating either owner', () => {
    const records: object[] = [];
    const value = { id: 'owned' };
    const length = new TextEncoder().encode(JSON.stringify(value)).byteLength;
    const budget = new FrontendBudget(16 * 1024 + length + 1023, () => ({
      register: (owner) => {
        records.push(owner);
      },
    }));
    expect(() => encodeWire(() => value, budget)).toThrow('RESOURCE_LIMIT');
    expect(records).toHaveLength(0);
    expect(budget.used).toBe(0);
  });
  it('matches JSON.stringify UTF-8 for Unicode, lone surrogates, escaping and protocol values', () => {
    const budget = new FrontendBudget();
    const unicode = Array.from({ length: 65536 }, (_, i) =>
      String.fromCharCode(i),
    ).join('');
    const values = [
      {
        name: unicode,
        optional: undefined,
        ids: ['A', '한글', '😀'],
        cursor: null,
      },
      {
        numbers: [-0, 1e-7, 1e21, Number.MAX_SAFE_INTEGER],
        enabled: true,
        disabled: false,
      },
      Object.assign(Object.create(null), {
        normal: 'ok',
        nested: [{ escaped: '"\\\n' }],
      }),
      Object.assign(new Array<unknown>(4), { 0: undefined, 1: null, 3: 'end' }),
    ];
    for (const value of values) {
      const actual = encodeWire(() => value, budget);
      const expected = new TextEncoder().encode(JSON.stringify(value));
      expect(actual.byteLength).toBe(expected.byteLength);
      expect(actual.findIndex((byte, index) => byte !== expected[index])).toBe(
        -1,
      );
    }
  });
  it('rejects oversized data before allocating a wire buffer and admits the exact byte limit', () => {
    const budget = new FrontendBudget();
    expect(encodeWire(() => 'x'.repeat(1_048_574), budget).byteLength).toBe(
      1_048_576,
    );
    const used = budget.used;
    expect(() => encodeWire(() => 'x'.repeat(1_048_575), budget)).toThrow(
      'RESOURCE_LIMIT',
    );
    expect(budget.used).toBe(used);
  });
  it('does not run getters, custom serializers or a factory without scratch admission', () => {
    const getter = vi.fn(() => 'secret');
    const custom = vi.fn(() => 'custom');
    const record = Object.defineProperty({}, 'value', {
      enumerable: true,
      get: getter,
    });
    const budget = new FrontendBudget();
    for (const value of [
      record,
      { toJSON: custom },
      new Date(),
      NaN,
      Infinity,
      1n,
    ])
      expect(() => encodeWire(() => value, budget)).toThrow('INVALID_ARGUMENT');
    expect(getter).not.toHaveBeenCalled();
    expect(custom).not.toHaveBeenCalled();
    expect(budget.used).toBe(0);
    const factory = vi.fn(() => ({}));
    expect(() => encodeWire(factory, new FrontendBudget(1024))).toThrow(
      'RESOURCE_LIMIT',
    );
    expect(factory).not.toHaveBeenCalled();
    const cycle: { child?: unknown } = {};
    cycle.child = cycle;
    expect(() => encodeWire(() => cycle, budget)).toThrow('INVALID_ARGUMENT');
    expect(budget.used).toBe(0);
  });
  it('tracks the backing buffer independently of its typed view', () => {
    const records = new Map<object, number>();
    let release!: (bytes: number) => void;
    const budget = new FrontendBudget(64 * 1024, (callback) => {
      release = callback;
      return {
        register: (owner, bytes) => {
          records.set(owner, bytes);
        },
      };
    });
    const view = encodeWire(() => ({ id: 'owned' }), budget);
    expect(budget.used).toBe(view.byteLength + 1024);
    release(records.get(view)!);
    expect(budget.used).toBe(view.byteLength + 512);
    release(records.get(view.buffer)!);
    expect(budget.used).toBe(0);
  });
});
