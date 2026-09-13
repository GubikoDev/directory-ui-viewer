import type { FrontendBudget } from './memory';

export interface ClientIssue {
  readonly message: string;
}

/** Diagnostic observations contain bounded protocol codes, never arbitrary
 * Error stacks/causes or filenames. Old snapshots retain their own GC charge. */
export class IssueLog {
  private readonly empty: readonly ClientIssue[];
  private readonly pressure: readonly ClientIssue[];
  private records: readonly ClientIssue[];
  constructor(private readonly budget: FrontendBudget) {
    const memory = budget.reserve(8192);
    try {
      this.empty = budget.retain([], memory);
      this.pressure = budget.retain([{ message: 'RESOURCE_LIMIT' }], memory);
      this.records = this.empty;
      budget.retainLifetimeFrom(this, memory, 2048);
    } finally {
      memory.close();
    }
  }
  get snapshot(): readonly ClientIssue[] {
    return this.records;
  }
  clear() {
    this.records = this.empty;
  }
  append(error: unknown) {
    let message = 'INTERNAL';
    if (error instanceof Error) {
      const field = Object.getOwnPropertyDescriptor(error, 'message');
      if (
        typeof field?.value === 'string' &&
        /^[A-Z][A-Z0-9_]{0,63}$/.test(field.value)
      )
        message = field.value;
    }
    try {
      this.records = this.budget.allocate(16 * 1024, () => {
        const next: ClientIssue[] = [];
        for (
          let i = Math.max(0, this.records.length - 31);
          i < this.records.length;
          i++
        )
          next.push(this.records[i]);
        next.push({ message });
        return next;
      });
    } catch {
      // Already admitted during construction; reporting pressure allocates no
      // replacement record or retry queue, even while the general pool is full.
      this.records = this.pressure;
    }
  }
}
