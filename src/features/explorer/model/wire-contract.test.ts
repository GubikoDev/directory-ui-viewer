import { expect, it } from 'vitest';
import fixture from './wire-fixture.json';
import {
  parseBytes,
  type DirectoryUsage,
  type RootData,
  type WorkRecord,
} from './types';

it('matches the exact Rust serde fixture including enum tags, optional fields and large integers', () => {
  const observedAt = '2026-09-13T00:00:00.000Z';
  const root: RootData = {
    session: {
      sessionId: 'fixture-session',
      generation: 1,
      rootEntryId: 'A',
      platform: 'fixture',
      capacity: { background: 'available', foreground: 'available' },
    },
    root: {
      entryId: 'A',
      parentId: null,
      displayName: 'A',
      kind: 'directory',
      hidden: { state: 'known', value: false },
      specialType: { state: 'unsupported' },
      modifiedAt: { state: 'known', value: observedAt },
      ownLogicalBytes: { state: 'known', value: '9007199254740993' },
      ownAllocatedBytes: { state: 'unknown' },
      observedAt,
      followPolicy: 'never',
    },
  };
  const work: WorkRecord = {
    sessionId: 'fixture-session',
    generation: 1,
    taskId: 'scan-1',
    operation: 'scanUsage',
    targetId: 'A',
    phase: 'queued',
    waitReason: 'draining',
    sequence: 1,
    processedEntries: '9007199254740993',
    processedDirectories: '0',
    observedAt,
    issues: {
      counts: { PERMISSION_DENIED: '1' },
      samples: [
        {
          code: 'PERMISSION_DENIED',
          operation: 'list',
          scope: 'entry',
          entryId: 'B',
          nativeCode: 13,
        },
      ],
    },
  };
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
    observedAt,
  };
  expect(fixture).toEqual({ root, work, usage });
  expect(parseBytes(fixture.work.processedEntries)).toBe(9007199254740993n);
});
