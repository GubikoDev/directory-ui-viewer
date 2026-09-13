import type { FrontendBudget } from './memory';

type Value = number | boolean | object;

/** Private mutable metadata. Charge insertions before Map growth; keep all
 * capacity/tombstone costs until the Map itself is collected. Delete/clear do
 * not allocate and never release a retained container's cost prematurely. */
export class TaskMap<T extends Value> {
  private map?: Map<string, T>;
  private compactPending = false;
  constructor(private readonly budget: FrontendBudget) {
    budget.retainLifetime(this, 512);
  }
  get size() {
    return this.map?.size ?? 0;
  }
  get(key: string) {
    return this.map?.get(key);
  }
  has(key: string) {
    return this.map?.has(key) ?? false;
  }
  forEach(visit: (value: T, key: string) => void) {
    this.map?.forEach(visit);
  }
  delete(key: string) {
    const map = this.map;
    if (!map) return false;
    const removed = map.delete(key);
    if (map.size === 0) this.map = undefined;
    return removed;
  }
  clear() {
    this.map = undefined;
    this.compactPending = false;
  }
  prune(keep: (key: string) => boolean) {
    const map = this.map;
    if (!map) return;
    for (const key of map.keys())
      if (!keep(key)) {
        map.delete(key);
        this.compactPending = true;
      }
    if (map.size === 0) {
      this.clear();
      return;
    }
    if (!this.compactPending) return;
    let bytes = 512;
    for (const key of map.keys()) bytes += 256 + key.length * 2;
    try {
      this.map = this.budget.allocateLifetime(bytes, () => new Map(map));
      this.compactPending = false;
    } catch {
      // Deletion is safe under full pressure. Retry capacity compaction on a
      // later state change, retaining the old Map's actual GC charge meanwhile.
    }
  }
  private prepare(key: string): Map<string, T> {
    const map = (this.map ??= this.budget.allocateLifetime(
      512,
      () => new Map<string, T>(),
    ));
    try {
      if (!map.has(key)) this.budget.growLifetime(map, 256 + key.length * 2);
    } catch (error) {
      if (map.size === 0) this.map = undefined;
      throw error;
    }
    return map;
  }
  set(key: string, value: T) {
    if (typeof value === 'object' && !this.budget.hasReservation(value))
      throw new Error('INVALID_RESERVATION');
    this.prepare(key).set(key, value);
  }
  run<R>(
    this: TaskMap<Promise<R>>,
    key: string,
    start: () => Promise<R>,
  ): Promise<R> {
    const existing = this.get(key);
    if (existing) return existing;
    if (this.size >= 64) throw new Error('RESOURCE_LIMIT');
    // Admission happens before starting the async operation. Its promise and
    // captured ID retain their charge after a generation clears the table.
    const map = this.prepare(key);
    let pending!: Promise<R>;
    try {
      pending = this.budget.allocateLifetime(4096 + key.length * 2, () =>
        start().finally(() => {
          // An older completion must never remove the replacement generation's job.
          if (this.get(key) === pending) this.delete(key);
        }),
      );
    } catch (error) {
      if (map.size === 0 && this.map === map) this.map = undefined;
      throw error;
    }
    map.set(key, pending);
    return pending;
  }
}
