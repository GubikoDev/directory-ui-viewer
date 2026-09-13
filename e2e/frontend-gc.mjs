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
    'model/types',
    'model/store',
    'adapters/fixture',
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
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
