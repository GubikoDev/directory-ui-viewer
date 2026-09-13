import type { SessionClient } from '../client/session-client';
import {
  sameScope,
  terminal,
  type Category,
  type Measure,
  type Scope,
} from '../model/types';

export interface ExpectedFixture {
  category: Category;
  firstPageEntries: number;
  entries: string;
  logical: string;
  allocated: string;
  partial: boolean;
}
export interface MeasurementClock {
  now(): number;
  wait(): Promise<void>;
  paint(): Promise<void>;
}
export interface TrialMeasurement {
  trial: number;
  startedAtUnixMs: number;
  cancelStartedAtUnixMs: number;
  refreshMs: number;
  firstPageMs: number;
  fullObservationMs: number;
  cancelMs: number;
  cancelStatus: 'accepted' | 'alreadyTerminal' | null;
  cancelPhaseBefore: string;
  entries: string;
  logicalState: Measure['state'];
  allocatedState: Measure['state'];
  logical: Measure;
  allocated: Measure;
  logicalMatches: boolean;
  allocatedMatches: boolean;
  entriesMatch: boolean;
  expectedPartial: boolean;
  firstPagePass: boolean | null;
  cancelLatencyPass: boolean | null;
  frontendUsed: number;
  frontendHighWaterSinceDocumentStart: number;
}
export interface MeasurementReport {
  layer: 'macos-app' | 'fixture';
  cache: 'OS cache uncontrolled; generation refreshed';
  trials: readonly TrialMeasurement[];
  stoppedBy: string | null;
  expected: ExpectedFixture;
}

/** Opt-in measurement of the existing client/adapter/UI path. No path grant,
 * debug native command, hidden rescan, filename logging or security shortcut. */
export function measureApprovedRoot(
  client: SessionClient,
  expected: ExpectedFixture,
  clock: MeasurementClock,
  signal: AbortSignal,
): Promise<MeasurementReport> {
  const budget = client.store.budget;
  return budget.allocateLifetime(32 * 1024, async () => {
    const original = client.store.getSnapshot().session;
    if (!original) throw new Error('SESSION_CLOSED');
    if (original.platform !== 'macos' && original.platform !== 'fixture')
      throw new Error('UNSUPPORTED_PLATFORM');
    const sessionId = original.sessionId;
    const layer = original.platform === 'macos' ? 'macos-app' : 'fixture';
    const trials = budget.allocateLifetime(
      4096,
      () => [] as TrialMeasurement[],
    );
    const check = (scope?: Scope) => {
      if (signal.aborted) throw new Error('MEASUREMENT_CANCELLED');
      const state = client.store.getSnapshot();
      if (
        state.session?.sessionId !== sessionId ||
        (scope && !sameScope(state.session, scope))
      )
        throw new Error('MEASUREMENT_INTERRUPTED');
      if (
        scope &&
        state.selectedId &&
        state.selectedId !== state.session?.rootEntryId
      )
        throw new Error('MEASUREMENT_INTERRUPTED');
      if (state.resourceLimited) throw new Error('RESOURCE_LIMIT');
    };
    const waitFor = async (scope: Scope, ready: () => boolean) => {
      const deadline = clock.now() + 120_000;
      for (;;) {
        check(scope);
        if (ready()) return;
        if (clock.now() >= deadline) throw new Error('MEASUREMENT_TIMEOUT');
        await budget.allocateLifetime(2048, clock.wait);
        await client.poll();
      }
    };
    let stoppedBy: string | null = null;
    try {
      for (let trial = 1; trial <= 10; trial++) {
        check();
        const startedAtUnixMs = Date.now();
        const refreshStarted = clock.now();
        await client.refresh();
        check();
        const refreshMs = clock.now() - refreshStarted;
        const root = client.store.getSnapshot().session!;
        const scope = budget.adopt({ sessionId, generation: root.generation });
        const rootId = root.rootEntryId;
        // refresh activates the default directory listing already. Include that
        // work in the clock; timing only another cached start would underreport.
        const started = refreshStarted;
        if (expected.category === 'files')
          await client.list(rootId, expected.category);
        await waitFor(scope, () => {
          const listing = client.store.getListing(rootId, expected.category);
          if (
            (listing.snapshot?.entryIds.length ?? 0) >=
            expected.firstPageEntries
          )
            return true;
          if (listing.loadState !== 'loading')
            throw new Error('INSUFFICIENT_FIRST_PAGE');
          return false;
        });
        await budget.allocateLifetime(2048, clock.paint);
        check(scope);
        const firstPageMs = clock.now() - started;
        await waitFor(scope, () => {
          const state = client.store.getSnapshot();
          for (const task of state.tasks.values())
            if (
              sameScope(task, scope) &&
              task.operation === 'scanUsage' &&
              terminal(task.phase)
            )
              return true;
          return false;
        });
        await client.readUsage(scope);
        check(scope);
        const fullObservationMs = clock.now() - refreshStarted;
        // Copy bounded results and already-owned root measures. Never keep an old whole snapshot
        // across the next generation or the cancellation measurement.
        const result = budget.allocateLifetime(16 * 1024, () => {
          const state = client.store.getSnapshot();
          const usage = state.usages.get(rootId);
          if (!usage) throw new Error('MISSING_USAGE');
          let entries = '';
          for (const task of state.tasks.values())
            if (sameScope(task, scope) && task.operation === 'scanUsage')
              entries = task.processedEntries;
          const matches = (value: Measure, bytes: string) =>
            expected.partial
              ? value.state === 'partial' &&
                value.reasons.includes('resourceLimit')
              : value.state === 'complete' && value.bytes === bytes;
          return {
            trial,
            startedAtUnixMs,
            cancelStartedAtUnixMs: 0,
            refreshMs,
            firstPageMs,
            fullObservationMs,
            cancelMs: 0,
            cancelStatus: null,
            cancelPhaseBefore: '',
            entries,
            logicalState: usage.logical.state,
            allocatedState: usage.allocated.state,
            logical: usage.logical,
            allocated: usage.allocated,
            logicalMatches: matches(usage.logical, expected.logical),
            allocatedMatches: matches(usage.allocated, expected.allocated),
            entriesMatch: expected.partial
              ? BigInt(entries) > 0n &&
                BigInt(entries) < BigInt(expected.entries)
              : entries === expected.entries,
            expectedPartial: expected.partial,
            firstPagePass:
              expected.firstPageEntries >= 200 ? firstPageMs <= 1000 : null,
            cancelLatencyPass: null,
            frontendUsed: budget.used,
            frontendHighWaterSinceDocumentStart: budget.peak,
          } as TrialMeasurement;
        });
        await client.refresh();
        check();
        const cancelScope = budget.adopt({
          sessionId,
          generation: client.store.getSnapshot().session!.generation,
        });
        let cancelId: string | undefined;
        for (const task of client.store.getSnapshot().tasks.values()) {
          if (sameScope(task, cancelScope) && task.operation === 'scanUsage') {
            cancelId = task.taskId;
            result.cancelPhaseBefore = task.phase;
            break;
          }
        }
        if (!cancelId) throw new Error('MISSING_USAGE_TASK');
        const cancelStarted = clock.now();
        result.cancelStartedAtUnixMs = Date.now();
        result.cancelStatus = (await client.cancel(cancelId)) ?? null;
        result.cancelMs = clock.now() - cancelStarted;
        check(cancelScope);
        result.cancelLatencyPass =
          result.cancelStatus === 'accepted' ? result.cancelMs <= 100 : null;
        result.frontendHighWaterSinceDocumentStart = budget.peak;
        // alreadyTerminal is reported, never counted as an accepted running cancel.
        trials.push(Object.freeze(result));
      }
    } catch (error) {
      // Retain completed trials on failure; never turn missing trials into a
      // successful run. Do not forward arbitrary error text/paths to reports.
      const code = error instanceof Error ? error.message : '';
      stoppedBy = /^[A-Z_]{1,64}$/.test(code) ? code : 'INTERNAL';
    }
    return budget.adopt({
      layer,
      cache: 'OS cache uncontrolled; generation refreshed',
      trials: Object.freeze(trials),
      stoppedBy,
      expected,
    } as MeasurementReport);
  });
}
