import { describe, expect, it, vi } from 'vitest';
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
  const store = new ExplorerStore(new FrontendBudget());
  store.reset(root);
  store.beginListing('A', 'directories', 1);
  store.registerListing(page(), 1);
  return store;
}
describe('ExplorerStore observations', () => {
  it('keeps the selection when the same wide branch is folded repeatedly', () => {
    const store = setup();
    expect(
      store.applyPage(
        page({
          entries: Array.from({ length: 1000 }, (_, i) =>
            fixtureEntry('dir-' + i, 'A'),
          ),
          nextCursor: null,
        }),
      ),
    ).toBe(true);
    store.select('A');
    const before = store.getSnapshot();
    const bytes = store.budget.used;
    for (let i = 0; i < 200; i++) store.select('A');
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(store.getSnapshot()).toBe(before);
    expect(store.budget.used).toBe(bytes);
    for (let i = 0; i < 200; i++) store.select(i % 2 ? 'A' : 'dir-0');
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(store.getSnapshot().selectedId).toBe('A');
  });
  it('can update one folder size repeatedly while a wide listing stays open', () => {
    const store = setup();
    expect(
      store.applyPage(
        page({
          entries: Array.from({ length: 1000 }, (_, i) =>
            fixtureEntry('dir-' + i, 'A'),
          ),
          nextCursor: null,
        }),
      ),
    ).toBe(true);
    for (let revision = 1; revision <= 200; revision++)
      store.applyUsage(root.session, [
        {
          directoryId: 'A',
          usageRevision: revision,
          scanState: 'running',
          logical: { state: 'unknown' },
          allocated: { state: 'unknown' },
          observedAt: work.observedAt,
        },
      ]);
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(store.getSnapshot().usages.get('A')?.usageRevision).toBe(200);
  });
  it('restores unused close capacity when validation throws before a close can allocate', () => {
    const store = setup();
    const held = store.getSnapshot();
    const pressure = store.budget.reserve(store.budget.available);
    const validation = vi
      .spyOn(store.budget, 'retain')
      .mockImplementationOnce(() => {
        throw new Error('INVALID_ARGUMENT');
      });
    expect(() => store.setExpanded('A', false)).toThrow('INVALID_ARGUMENT');
    validation.mockRestore();
    expect(store.getSnapshot()).toBe(held);
    expect(store.budget.used).toBe(store.budget.limit);
    const reservation = vi
      .spyOn(store.budget, 'reserve')
      .mockImplementation(() => {
        throw new Error('unexpected external reservation');
      });
    expect(store.setExpanded('A', false)).toBe(true);
    expect(reservation).not.toHaveBeenCalled();
    expect(held.expandedIds.has('A')).toBe(true);
    reservation.mockRestore();
    pressure.close();
  });
  it('rolls back an open when its closing allowance cannot grow and preserves earlier close guarantees', () => {
    const store = setup();
    expect(
      store.applyPage(
        page({
          entries: [fixtureEntry('B', 'A'), fixtureEntry('C', 'A')],
          nextCursor: null,
        }),
      ),
    ).toBe(true);
    expect(store.beginListing('B', 'files', 2)).toBe(true);
    const before = store.getSnapshot();
    const reserve = store.budget.reserve.bind(store.budget);
    let pressure!: ReturnType<typeof reserve>;
    const admission = vi
      .spyOn(store.budget, 'reserve')
      .mockImplementationOnce((bytes, priority) => {
        const memory = reserve(bytes, priority);
        pressure = reserve(store.budget.available);
        return memory;
      });
    expect(store.beginListing('C', 'files', 3)).toBe(false);
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(store.getSnapshot().openFileListIds.has('C')).toBe(false);
    expect(store.isListingRequestCurrent('C', 'files', 3)).toBe(false);
    expect(store.getSnapshot().openFileListIds.has('B')).toBe(true);
    admission.mockRestore();
    const rest = reserve(store.budget.available);
    const closed = vi.spyOn(store.budget, 'reserve').mockImplementation(() => {
      throw new Error('unexpected external reservation');
    });
    expect(store.setExpanded('B', false, 'files')).toBe(true);
    expect(closed).not.toHaveBeenCalled();
    expect(store.getSnapshot().openFileListIds.has('B')).toBe(false);
    expect(before.openFileListIds.has('B')).toBe(true);
    closed.mockRestore();
    rest.close();
    pressure.close();
  });
  it('closes every admitted listing under full pressure without new reservations or changing held snapshots', () => {
    const budget = new FrontendBudget(32 * 1024 * 1024, undefined, 1024 * 1024);
    const store = new ExplorerStore(budget);
    store.reset(root);
    store.beginListing('A', 'directories', 1);
    store.registerListing(page(), 1);
    const ids = ['B', 'C', 'D'];
    expect(
      store.applyPage(
        page({
          entries: ids.map((id) => fixtureEntry(id, 'A')),
          nextCursor: null,
        }),
      ),
    ).toBe(true);
    for (const [i, id] of ids.entries()) {
      const files = page({
        directoryId: id,
        category: 'files',
        listingRevision: id,
        work: { ...work, taskId: id, operation: 'listFiles', targetId: id },
        entries: [fixtureEntry(`${id}-file`, id, 'regularFile')],
        nextCursor: null,
      });
      expect(store.beginListing(id, 'files', i + 2)).toBe(true);
      expect(store.registerListing(files, i + 2)).toBe(true);
      expect(store.applyPage(files)).toBe(true);
    }
    store.select('B');
    const held = store.getSnapshot();
    const pressure = budget.reserve(budget.available);
    const control = budget.reserve(1024 * 1024, true);
    expect(budget.used).toBe(budget.limit);
    const allocation = vi.spyOn(budget, 'reserve').mockImplementation(() => {
      throw new Error('unexpected external reservation');
    });
    for (const [i, id] of ids.entries()) {
      expect(store.setExpanded(id, false, 'files')).toBe(true);
      store.evictClosedListings();
      expect(store.getSnapshot().openFileListIds.has(id)).toBe(false);
      expect(store.getSnapshot().entries.has(`${id}-file`)).toBe(false);
      for (const open of ids.slice(i + 1)) {
        expect(store.getSnapshot().openFileListIds.has(open)).toBe(true);
        expect(store.getSnapshot().entries.has(`${open}-file`)).toBe(true);
      }
      expect(store.getSnapshot().selectedId).toBe('B');
      expect(store.getSnapshot().entries.has('B')).toBe(true);
      expect(held.entries.has(`${id}-file`)).toBe(true);
      expect(held.openFileListIds.has(id)).toBe(true);
      expect(budget.used).toBeLessThanOrEqual(budget.limit);
    }
    expect(store.setExpanded('A', false)).toBe(true);
    expect(store.setExpanded('A', false)).toBe(true);
    expect(allocation).not.toHaveBeenCalled();
    allocation.mockRestore();
    control.close();
    pressure.close();
  });
  it('fits fully visible retention scratch inside the admitted usage transaction', () => {
    const store = setup();
    expect(
      store.applyPage(
        page({
          entries: Array.from({ length: 1000 }, (_, i) =>
            fixtureEntry(`visible-${i}`, 'A'),
          ),
          nextCursor: null,
        }),
      ),
    ).toBe(true);
    const usage: DirectoryUsage = {
      directoryId: 'A',
      usageRevision: 1,
      scanState: 'settled',
      logical: { state: 'complete', bytes: '123' },
      allocated: { state: 'complete', bytes: '4096' },
      observedAt: work.observedAt,
    };
    const reserve = store.budget.reserve.bind(store.budget);
    let pressure!: ReturnType<typeof reserve>;
    const admitted = vi
      .spyOn(store.budget, 'reserve')
      .mockImplementationOnce((bytes, priority) => {
        const memory = reserve(bytes, priority);
        // No unreserved pool remains after admission: all publish scratch must
        // fit the existing transaction allowance, without raising any limit.
        pressure = reserve(store.budget.available);
        return memory;
      });
    store.applyUsage(root.session, [usage]);
    expect(admitted).toHaveBeenCalledOnce();
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(store.getSnapshot().entries.size).toBe(1001);
    expect(store.getSnapshot().usages.get('A')).toEqual(usage);
    pressure.close();
    admitted.mockRestore();
  });
  it('reclaims obsolete revision entries and tasks while keeping the current open listing', () => {
    const store = setup();
    store.applyPage(
      page({ entries: [fixtureEntry('old', 'A')], nextCursor: null }),
    );
    const previous = store.getSnapshot();
    const replacement = page({
      work: { ...work, taskId: 'replacement' },
      listingRevision: 'replacement-revision',
      entries: [fixtureEntry('new', 'A')],
      nextCursor: null,
    });
    expect(store.beginListing('A', 'directories', 2)).toBe(true);
    expect(store.registerListing(replacement, 2)).toBe(true);
    expect(store.applyPage(replacement)).toBe(true);
    const pressure = store.budget.reserve(store.budget.available);
    store.setMode('mindmap');
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect([...store.getSnapshot().entries.keys()].sort()).toEqual([
      'A',
      'new',
    ]);
    expect([...store.getSnapshot().tasks.keys()]).toEqual(['replacement']);
    expect(store.getListing('A', 'directories').snapshot?.entryIds).toEqual([
      'new',
    ]);
    expect(previous.entries.has('old')).toBe(true);
    expect(previous.tasks.has('t')).toBe(true);
    pressure.close();
  });
  it('evicts closed payloads without allocation under pressure and explicitly resumes after release', () => {
    const budget = new FrontendBudget();
    const store = new ExplorerStore(budget);
    store.reset(root);
    store.beginListing('A', 'directories', 1);
    store.registerListing(page(), 1);
    store.applyPage(
      page({
        entries: [fixtureEntry('B', 'A')],
        nextCursor: null,
        coverage: 'complete',
        work: { ...work, phase: 'completed', sequence: 2 },
      }),
    );
    const files = page({
      directoryId: 'B',
      category: 'files',
      listingRevision: 'files',
      entries: [fixtureEntry('closed-file', 'B', 'regularFile')],
      nextCursor: null,
      coverage: 'complete',
      work: {
        ...work,
        taskId: 'files',
        operation: 'listFiles',
        targetId: 'B',
        phase: 'completed',
      },
    });
    store.beginListing('B', 'files', 2);
    store.registerListing(files, 2);
    expect(store.applyPage(files)).toBe(true);
    store.setExpanded('B', false, 'files');
    store.select('A');
    const held = store.getSnapshot();
    const blocker = budget.reserve(budget.available);
    store.applyTask({ ...work, phase: 'completed', sequence: 3 });
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(store.getSnapshot().entries.has('closed-file')).toBe(false);
    expect(store.getListing('B', 'files').loadState).toBe('notRequested');
    expect(store.getSnapshot().selectedId).toBe('A');
    expect(store.getSnapshot().entries.has('B')).toBe(true);
    expect(held.entries.has('closed-file')).toBe(true);
    expect(budget.used).toBe(budget.limit);
    expect(store.resumeObservations()).toBe(false);
    expect(store.registerListing(files, 2)).toBe(false);
    expect(store.applyPage(files)).toBe(false);
    blocker.close();
    expect(store.resumeObservations()).toBe(true);
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(store.beginListing('B', 'files', 3)).toBe(true);
    expect(store.registerListing(files, 2)).toBe(false);
    expect(store.registerListing(files, 3)).toBe(true);
    expect(store.applyPage(files)).toBe(true);
  });
  it('preserves selected descendants and ancestors when their closed sibling payloads are reclaimed', () => {
    const store = setup();
    store.applyPage(
      page({
        entries: [fixtureEntry('B', 'A')],
        nextCursor: null,
        work: { ...work, phase: 'completed', sequence: 2 },
      }),
    );
    const files = page({
      directoryId: 'B',
      category: 'files',
      listingRevision: 'files',
      entries: [
        fixtureEntry('selected-file', 'B', 'regularFile'),
        fixtureEntry('other-file', 'B', 'regularFile'),
      ],
      nextCursor: null,
      work: {
        ...work,
        taskId: 'files',
        operation: 'listFiles',
        targetId: 'B',
        phase: 'completed',
      },
    });
    store.beginListing('B', 'files', 2);
    store.registerListing(files, 2);
    store.applyPage(files);
    store.select('selected-file');
    store.setExpanded('B', false, 'files');
    expect(store.evictClosedListings()).toBe(true);
    expect([...store.getSnapshot().entries.keys()].sort()).toEqual([
      'A',
      'B',
      'selected-file',
    ]);
    expect(store.getSnapshot().selectedId).toBe('selected-file');
    expect(store.getSnapshot().tasks.has('files')).toBe(false);
    store.select('B');
    store.suspendObservations();
    expect(store.getSnapshot().entries.has('selected-file')).toBe(false);
    expect(store.getSnapshot().entries.has('B')).toBe(true);
  });
  it('distinguishes unrequested, pending empty, true empty, and permission failure', () => {
    const store = new ExplorerStore();
    store.reset(root);
    expect(store.getListing('A', 'directories').loadState).toBe('notRequested');
    store.beginListing('A', 'directories', 1);
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
    store.beginListing('A', 'directories', 3);
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
    let owned = 0;
    const budget = new FrontendBudget(32 * 1024 * 1024, () => ({
      register(_target, bytes) {
        owned += bytes;
      },
    }));
    const store = new ExplorerStore(budget);
    store.reset(root);
    store.beginListing('A', 'directories', 1);
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
    // Reset returns unused close capacity, but no object's GC charge. This
    // collector has released nothing, including the externally held snapshot.
    expect(budget.used).toBe(blocker.bytes + owned);
    expect(before.entries.has('A')).toBe(true);
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
    const scan: WorkRecord = { ...work, operation: 'scanUsage' };
    const budget = new FrontendBudget();
    const store = new ExplorerStore(budget);
    store.reset(root);
    const baseline = budget.used;
    expect(store.applyTask(scan)).toBe(true);
    expect(budget.used).toBeGreaterThan(baseline);
    const old = store.getSnapshot();
    expect(store.applyTask({ ...scan, sequence: 2 })).toBe(true);
    expect(old.tasks.get('t')?.sequence).toBe(1);
    expect(store.getSnapshot().tasks.get('t')?.sequence).toBe(2);
    const beforeUsage = budget.used;
    const usage: DirectoryUsage = {
      directoryId: 'A',
      usageRevision: 1,
      scanState: 'settled',
      logical: { state: 'complete', bytes: '123' },
      allocated: { state: 'complete', bytes: '4096' },
      observedAt: scan.observedAt,
    };
    store.applyUsage(root.session, [usage]);
    expect(budget.used).toBeGreaterThan(beforeUsage);
    const before = store.getSnapshot();
    const blocker = budget.reserve(budget.limit - budget.used);
    store.applyUsage(root.session, [{ ...usage, usageRevision: 2 }]);
    expect(store.getSnapshot().usages).toBe(before.usages);
    expect(store.applyTask({ ...scan, taskId: 'new', sequence: 3 })).toBe(
      false,
    );
    expect(store.getSnapshot().tasks).toBe(before.tasks);
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(budget.peak).toBe(budget.limit);
    blocker.close();
    expect(store.applyTask({ ...scan, sequence: 3 })).toBe(true);
    expect(old.tasks.get('t')?.sequence).toBe(1);
  });
});
