import { describe, expect, it } from 'vitest';
import { fixtureEntry } from '../adapters/fixture';
import { ExplorerStore } from './store';
import { FrontendBudget } from '../runtime/memory';
import {
  parseBytes,
  type DirectoryUsage,
  type ListingPage,
  type RootData,
  type WorkRecord,
} from './types';
const root: RootData = {
  root: fixtureEntry('A', null),
  session: {
    sessionId: 's',
    generation: 1,
    rootEntryId: 'A',
    platform: 'fixture',
    capacity: { background: 'available', foreground: 'available' },
  },
};
const work: WorkRecord = {
  sessionId: 's',
  generation: 1,
  taskId: 't',
  operation: 'listDirectories',
  targetId: 'A',
  phase: 'running',
  sequence: 1,
  processedEntries: '0',
  processedDirectories: '0',
  observedAt: '2026-09-13T00:00:00Z',
  issues: { counts: {}, samples: [] },
};
const page = (changes: Partial<ListingPage> = {}): ListingPage => ({
  work,
  directoryId: 'A',
  category: 'directories',
  listingRevision: 'r',
  cursor: null,
  entries: [],
  nextCursor: 'cursor-0',
  coverage: 'partial',
  issues: work.issues,
  ...changes,
});
function setup() {
  const store = new ExplorerStore();
  store.reset(root);
  store.registerListing(page(), 1);
  return store;
}
describe('ExplorerStore observations', () => {
  it('distinguishes unrequested, pending empty, true empty, and permission failure', () => {
    const store = new ExplorerStore();
    store.reset(root);
    expect(store.getListing('A', 'directories').loadState).toBe('notRequested');
    store.registerListing(page(), 1);
    store.applyPage(page());
    expect(store.getListing('A', 'directories').loadState).toBe('loading');
    store.applyPage(
      page({
        cursor: 'cursor-0',
        nextCursor: null,
        work: { ...work, phase: 'completed', sequence: 2 },
        coverage: 'complete',
      }),
    );
    expect(store.getListing('A', 'directories')).toMatchObject({
      loadState: 'settled',
      snapshot: { entryIds: [], coverage: 'complete' },
    });
    const failed = setup();
    failed.applyPage(
      page({
        nextCursor: null,
        work: { ...work, phase: 'failed', sequence: 2 },
        coverage: 'partial',
        issues: { counts: { PERMISSION_DENIED: '1' }, samples: [] },
      }),
    );
    expect(failed.getListing('A', 'directories')).toMatchObject({
      loadState: 'failed',
      snapshot: {
        coverage: 'partial',
        issues: { counts: { PERMISSION_DENIED: '1' } },
      },
    });
  });
  it('rejects previous generations, stale revisions and mismatched parent/category', () => {
    const store = setup();
    expect(
      store.applyPage(
        page({
          work: { ...work, generation: 0 },
          entries: [fixtureEntry('B', 'A')],
        }),
      ),
    ).toBe(false);
    expect(store.applyPage(page({ listingRevision: 'old' }))).toBe(false);
    expect(
      store.applyPage(page({ entries: [fixtureEntry('B', 'outside')] })),
    ).toBe(false);
    expect(
      store.applyPage(
        page({ entries: [fixtureEntry('link', 'A', 'symlink')] }),
      ),
    ).toBe(false);
    expect(store.getSnapshot().entries.size).toBe(1);
  });
  it('ignores reordered notifications and never resurrects terminal work', () => {
    const store = setup();
    const event = {
      protocolVersion: 1 as const,
      ...work,
      sequence: 3,
      kind: 'progress' as const,
    };
    expect(store.acceptEvent(event)).toBe(true);
    expect(store.acceptEvent(event)).toBe(false);
    expect(store.acceptEvent({ ...event, taskId: 'unregistered' })).toBe(false);
    store.applyTask({ ...work, phase: 'completed', sequence: 4 });
    expect(store.applyTask({ ...work, sequence: 5 })).toBe(false);
    expect(store.acceptEvent({ ...event, sequence: 6 })).toBe(false);
  });
  it('does not apply a delayed running page after terminal cancellation', () => {
    const store = setup();
    store.applyTask({ ...work, phase: 'cancelled', sequence: 4 });
    expect(store.applyPage(page({ entries: [fixtureEntry('B', 'A')] }))).toBe(
      false,
    );
    expect(store.getSnapshot().entries.size).toBe(1);
  });

  it('rejects same-scope late listing starts by local request order', () => {
    const store = setup();
    store.registerListing(
      page({ listingRevision: 'r2', work: { ...work, taskId: 't2' } }),
      3,
    );
    expect(
      store.registerListing(
        page({ listingRevision: 'r1', work: { ...work, taskId: 't1' } }),
        2,
      ),
    ).toBe(false);
    expect(store.getListing('A', 'directories').snapshot?.listingRevision).toBe(
      'r2',
    );
  });
  it('rejects conflicting same-sequence task state and accepts terminal notification during cancellation', () => {
    const store = setup();
    expect(store.applyTask({ ...work, phase: 'failed' })).toBe(false);
    store.markCancelPending(work.taskId);
    expect(
      store.acceptEvent({
        protocolVersion: 1,
        ...work,
        sequence: 2,
        kind: 'terminal',
      }),
    ).toBe(true);
  });

  it('keeps independent usage completeness and ignores reversed responses', () => {
    const store = setup();
    const usage: DirectoryUsage = {
      directoryId: 'A',
      usageRevision: 2,
      scanState: 'settled',
      logical: { state: 'complete', bytes: '9007199254740993' },
      allocated: {
        state: 'partial',
        observedBytes: '4096',
        reasons: ['unsupported'],
      },
      observedAt: work.observedAt,
    };
    store.applyUsage(root.session, [usage]);
    store.applyUsage(root.session, [
      { ...usage, usageRevision: 1, logical: { state: 'unknown' } },
    ]);
    expect(store.getSnapshot().usages.get('A')).toEqual(usage);
    expect(store.getListing('A', 'directories').loadState).toBe('loading');
    expect(parseBytes('9007199254740993')).toBe(9007199254740993n);
    for (const value of ['-1', '01', '1.1', '1e3', ''])
      expect(() => parseBytes(value)).toThrow('INVALID_BYTES');
  });
  it('keeps cancelled observations partial and stops accepting progress', () => {
    const store = setup();
    store.markCancelPending('t');
    expect(store.applyTask({ ...work, sequence: 2 })).toBe(false);
    store.applyPage(
      page({
        work: { ...work, phase: 'cancelled', sequence: 3 },
        nextCursor: null,
        entries: [fixtureEntry('B', 'A')],
      }),
    );
    expect(store.getListing('A', 'directories')).toMatchObject({
      loadState: 'cancelled',
      snapshot: { coverage: 'partial', entryIds: ['B'] },
    });
  });
  it('switches views using identical normalized objects without losing expansion', () => {
    const store = setup();
    store.setExpanded('A', true);
    const before = store.getSnapshot();
    store.setMode('mindmap');
    expect(store.getSnapshot().entries).toBe(before.entries);
    expect(store.getSnapshot().expandedIds.has('A')).toBe(true);
  });
  it('signals memory pressure before adding an oversized page', () => {
    const budget = new FrontendBudget();
    const store = new ExplorerStore(budget);
    store.reset(root);
    store.registerListing(page(), 1);
    const before = store.getSnapshot();
    const blocker = budget.reserve(budget.limit - budget.used);
    expect(store.applyPage(page({ entries: [fixtureEntry('B', 'A')] }))).toBe(
      false,
    );
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(store.getSnapshot().entries.size).toBe(1);
    expect(store.getSnapshot().entries).toBe(before.entries);
    expect(store.getSnapshot().tasks).toBe(before.tasks);
    expect(budget.used).toBe(budget.limit);
    store.reset(null);
    expect(store.getSnapshot().session).toBeNull();
    expect(store.getSnapshot().entries.size).toBe(0);
    // Closing cannot free old snapshots still held by React/a caller.
    expect(budget.used).toBe(budget.limit);
    blocker.close();
  });
  it('does not reserve retained issue payloads again when only a task-map slot changes', () => {
    const budget = new FrontendBudget();
    const store = new ExplorerStore(budget);
    store.reset(root);
    const first: WorkRecord = {
      ...work,
      issues: {
        counts: { PERMISSION_DENIED: '32' },
        samples: Array.from({ length: 32 }, () => ({
          code: 'PERMISSION_DENIED',
          operation: 'x'.repeat(64),
          scope: 'entry',
          entryId: 'e'.repeat(256),
          nativeCode: 13,
        })),
      },
    };
    expect(store.applyTask(first)).toBe(true);
    const next = budget.adopt({ ...first, sequence: 2 });
    const blocker = budget.reserve(budget.limit - budget.used - 64 * 1024);
    expect(store.applyTask(next)).toBe(true);
    expect(store.getSnapshot().tasks.get('t')?.sequence).toBe(2);
    expect(store.getSnapshot().resourceLimited).toBe(false);
    blocker.close();
  });
  it('charges task and usage maps, preserves older snapshots and rejects both under shared pressure', () => {
    const budget = new FrontendBudget();
    const store = new ExplorerStore(budget);
    store.reset(root);
    const baseline = budget.used;
    expect(store.applyTask(work)).toBe(true);
    expect(budget.used).toBeGreaterThan(baseline);
    const old = store.getSnapshot();
    expect(store.applyTask({ ...work, sequence: 2 })).toBe(true);
    expect(old.tasks.get('t')?.sequence).toBe(1);
    expect(store.getSnapshot().tasks.get('t')?.sequence).toBe(2);
    const beforeUsage = budget.used;
    const usage: DirectoryUsage = {
      directoryId: 'A',
      usageRevision: 1,
      scanState: 'settled',
      logical: { state: 'complete', bytes: '123' },
      allocated: { state: 'complete', bytes: '4096' },
      observedAt: work.observedAt,
    };
    store.applyUsage(root.session, [usage]);
    expect(budget.used).toBeGreaterThan(beforeUsage);
    const before = store.getSnapshot();
    const blocker = budget.reserve(budget.limit - budget.used);
    store.applyUsage(root.session, [{ ...usage, usageRevision: 2 }]);
    expect(store.getSnapshot().usages).toBe(before.usages);
    expect(store.applyTask({ ...work, taskId: 'new', sequence: 3 })).toBe(
      false,
    );
    expect(store.getSnapshot().tasks).toBe(before.tasks);
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(budget.peak).toBe(budget.limit);
    blocker.close();
    expect(store.applyTask({ ...work, sequence: 3 })).toBe(true);
    expect(old.tasks.get('t')?.sequence).toBe(1);
  });
});
