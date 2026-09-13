// Real Node/V8 collection evidence. Does not substitute for WKWebView validation.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import ts from 'typescript';

assert.equal(typeof global.gc, 'function', 'run with --expose-gc');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'duv-store-gc-'));
try {
  // Compile only repository-owned modules into an ephemeral isolated ESM tree.
  for (const file of [
    'runtime/memory',
    'runtime/task-map',
    'runtime/issue-log',
    'model/types',
    'model/store',
    'model/retention',
    'adapters/fixture',
    'adapters/encoding',
    'adapters/inbox',
    'adapters/events',
  ]) {
    const source = fs.readFileSync(
      new URL(`../src/features/explorer/${file}.ts`, import.meta.url),
      'utf8',
    );
    const compiled = ts
      .transpileModule(source, {
        compilerOptions: {
          target: ts.ScriptTarget.ES2022,
          module: ts.ModuleKind.ESNext,
        },
      })
      .outputText.replace(
        /from (['"])(\.\.?\/[^'"]+)\1/g,
        (_match, quote, specifier) => `from ${quote}${specifier}.mjs${quote}`,
      );
    const target = path.join(dir, `${file}.mjs`);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, compiled);
  }
  const load = (file) => import(pathToFileURL(path.join(dir, `${file}.mjs`)));
  const { FrontendBudget } = await load('runtime/memory');
  const { ExplorerStore } = await load('model/store');
  const { TaskMap } = await load('runtime/task-map');
  const { IssueLog } = await load('runtime/issue-log');
  const { ReplyInbox } = await load('adapters/inbox');
  const { WorkInbox } = await load('adapters/events');
  const { encodeWire } = await load('adapters/encoding');
  const { fixtureEntry } = await load('adapters/fixture');
  const budget = new FrontendBudget();
  const store = new ExplorerStore(budget);
  async function settle() {
    for (let i = 0; i < 12; i++) {
      global.gc();
      await setTimeout(10);
    }
  }
  await settle();
  const baseline = budget.used;
  (() => {
    const root = budget.parse(
      JSON.stringify({
        session: {
          sessionId: 'owned-session',
          generation: 1,
          rootEntryId: 'A',
          platform: 'fixture',
          capacity: { background: 'available', foreground: 'available' },
        },
        root: fixtureEntry('A', null),
      }),
    );
    assert.equal(store.reset(root), true);
  })();
  let retained = store.getSnapshot();
  store.reset(null);
  await settle();
  const whileRetained = budget.used;
  assert(whileRetained > baseline);
  assert.equal(retained.entries.get('A').displayName, 'A');
  retained = undefined;
  for (let i = 0; i < 200 && budget.used !== baseline; i++) {
    global.gc();
    await setTimeout(10);
  }
  assert.equal(budget.used, baseline);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      baseline,
      whileRetained,
      afterRelease: budget.used,
      peak: budget.peak,
    }),
  );
  const jobBudget = new FrontendBudget();
  const jobs = new TaskMap(jobBudget);
  const jobBaseline = jobBudget.used;
  let finish;
  let pending = jobs.run(
    'job',
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  jobs.clear();
  await settle();
  const whilePending = jobBudget.used;
  assert(whilePending >= jobBaseline + 4096 + 6);
  finish();
  await pending;
  await settle();
  const whilePromiseRetained = jobBudget.used;
  assert(whilePromiseRetained >= jobBaseline + 4096 + 6);
  pending = undefined;
  finish = undefined;
  for (let i = 0; i < 200 && jobBudget.used !== jobBaseline; i++) {
    global.gc();
    await setTimeout(10);
  }
  assert.equal(jobs.size, 0);
  assert.equal(jobBudget.used, jobBaseline);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      jobBaseline,
      whilePending,
      whilePromiseRetained,
      afterPromiseRelease: jobBudget.used,
      peak: jobBudget.peak,
    }),
  );
  const wireBudget = new FrontendBudget();
  let view = encodeWire(() => ({ ids: ['owned', '한글', '😀'] }), wireBudget);
  let backing = view.buffer;
  const length = view.byteLength;
  assert.equal(wireBudget.used, length + 1024);
  view = undefined;
  await settle();
  const whileBufferRetained = wireBudget.used;
  assert.equal(whileBufferRetained, length + 512);
  assert.equal(new Uint8Array(backing)[0], 123);
  backing = undefined;
  for (let i = 0; i < 200 && wireBudget.used !== 0; i++) {
    global.gc();
    await setTimeout(10);
  }
  assert.equal(wireBudget.used, 0);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      wireBytes: length,
      whileBufferRetained,
      afterBufferRelease: wireBudget.used,
      peak: wireBudget.peak,
    }),
  );
  const replyBudget = new FrontendBudget();
  let inbox = new ReplyInbox(undefined, 2, replyBudget);
  const inboxBaseline = replyBudget.used;
  inbox.open('owned', false, 256 * 1024);
  const pressure = replyBudget.reserve(replyBudget.available);
  assert(inbox.receive('owned', '{"session":{"sessionId":"owned"}}'));
  let received = inbox.parse('owned');
  inbox.close('owned');
  pressure.close();
  await settle();
  const whileReplyRetained = replyBudget.used;
  assert(whileReplyRetained > inboxBaseline);
  assert.equal(received.session.sessionId, 'owned');
  received = undefined;
  await settle();
  assert.equal(replyBudget.used, inboxBaseline);
  inbox = undefined;
  await settle();
  assert.equal(replyBudget.used, 0);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      inboxBaseline,
      whileReplyRetained,
      afterInboxRelease: replyBudget.used,
    }),
  );

  const issueBudget = new FrontendBudget();
  let issues = new IssueLog(issueBudget);
  const issueBaseline = issueBudget.used;
  issues.append(new Error('IO_ERROR'));
  let issueSnapshot = issues.snapshot;
  issues.clear();
  await settle();
  const whileIssueRetained = issueBudget.used;
  assert(whileIssueRetained > issueBaseline);
  assert.equal(issueSnapshot[0].message, 'IO_ERROR');
  issueSnapshot = undefined;
  await settle();
  assert.equal(issueBudget.used, issueBaseline);
  issues = undefined;
  await settle();
  assert.equal(issueBudget.used, 0);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      issueBaseline,
      whileIssueRetained,
      afterIssueLogRelease: issueBudget.used,
    }),
  );

  const eventBudget = new FrontendBudget();
  let events = new WorkInbox(eventBudget);
  let unsubscribe = events.subscribe(() => {});
  unsubscribe();
  events = undefined;
  await settle();
  const whileStopRetained = eventBudget.used;
  assert.equal(whileStopRetained, 3072);
  unsubscribe = undefined;
  await settle();
  assert.equal(eventBudget.used, 0);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      whileStopRetained,
      afterStopRelease: eventBudget.used,
    }),
  );
  const retainedBudget = new FrontendBudget();
  const compactStore = new ExplorerStore(retainedBudget);
  await settle();
  const compactBaseline = retainedBudget.used;
  let closedSnapshot = (() => {
    const scope = { sessionId: 'owned', generation: 1 };
    compactStore.reset({
      root: fixtureEntry('A', null),
      session: {
        ...scope,
        rootEntryId: 'A',
        platform: 'fixture',
        capacity: { foreground: 'available', background: 'available' },
      },
    });
    const issues = { counts: {}, samples: [] };
    function publish(id, category, entries, order) {
      const work = {
        ...scope,
        taskId: 'task-' + order,
        operation: category === 'files' ? 'listFiles' : 'listDirectories',
        targetId: id,
        phase: 'completed',
        sequence: 1,
        processedEntries: String(entries.length),
        processedDirectories: '0',
        observedAt: '2026-09-14T00:00:00Z',
        issues,
      };
      const page = {
        work,
        directoryId: id,
        category,
        listingRevision: 'revision-' + order,
        entries,
        cursor: null,
        nextCursor: null,
        coverage: 'complete',
        issues,
      };
      assert(compactStore.beginListing(id, category, order));
      assert(compactStore.registerListing(page, order));
      assert(compactStore.applyPage(page));
    }
    publish('A', 'directories', [fixtureEntry('B', 'A')], 1);
    publish(
      'B',
      'files',
      Array.from({ length: 300 }, (_, i) =>
        fixtureEntry('file-' + i, 'B', 'regularFile'),
      ),
      2,
    );
    compactStore.setExpanded('B', false, 'files');
    const held = compactStore.getSnapshot();
    const pressure = retainedBudget.reserve(retainedBudget.available);
    compactStore.setMode('mindmap');
    assert.equal(compactStore.getSnapshot().entries.size, 2);
    assert.equal(held.entries.size, 302);
    assert.equal(retainedBudget.used, retainedBudget.limit);
    pressure.close();
    assert(compactStore.resumeObservations());
    return held;
  })();
  await settle();
  const whileClosedSnapshotRetained = retainedBudget.used;
  assert.equal(closedSnapshot.entries.size, 302);
  closedSnapshot = undefined;
  await settle();
  const afterClosedSnapshotRelease = retainedBudget.used;
  assert(afterClosedSnapshotRelease < whileClosedSnapshotRetained);
  assert.equal(compactStore.getSnapshot().entries.size, 2);
  compactStore.reset(null);
  await settle();
  assert.equal(retainedBudget.used, compactBaseline);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      compactBaseline,
      whileClosedSnapshotRetained,
      afterClosedSnapshotRelease,
      afterCompactStoreReset: retainedBudget.used,
    }),
  );
  // Abandon an open store without explicit reset. Both its live-data owners and
  // unused closing allowance must disappear when that store is collected.
  const abandonedBudget = new FrontendBudget();
  let abandonedStore = new ExplorerStore(abandonedBudget);
  abandonedStore.reset({
    root: fixtureEntry('A', null),
    session: {
      sessionId: 'abandoned',
      generation: 1,
      rootEntryId: 'A',
      platform: 'fixture',
      capacity: { foreground: 'available', background: 'available' },
    },
  });
  assert(abandonedStore.beginListing('A', 'files', 1));
  await settle();
  const whileOpenStoreRetained = abandonedBudget.used;
  assert(whileOpenStoreRetained > 0);
  abandonedStore = undefined;
  for (let i = 0; i < 200 && abandonedBudget.used !== 0; i++) {
    global.gc();
    await setTimeout(10);
  }
  assert.equal(abandonedBudget.used, 0);
  console.log(
    JSON.stringify({
      engine: 'Node V8; not WKWebView',
      whileOpenStoreRetained,
      afterOpenStoreRelease: abandonedBudget.used,
    }),
  );
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
