// Creates only a fresh owned temporary fixture. No existing/user path input.
// Keep selectionPath for the native picker; paste only expectation into the
// opt-in measurement panel. Fixture preparation is outside measured latency.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import profiles from '../src/features/explorer/verification/profiles.json' with { type: 'json' };

const profile = process.argv[2];
if (process.argv.length !== 3 || !Object.hasOwn(profiles, profile)) {
  throw new Error(
    'Choose one fixed profile: flat10k, flat100k, wide, deep, hardlinks',
  );
}
const root = fs.mkdtempSync(path.join(os.tmpdir(), `duv-app-${profile}-`));
try {
  if (profile === 'flat10k' || profile === 'flat100k') {
    for (let i = 0; i < Number(profiles[profile].entries); i++)
      fs.closeSync(
        fs.openSync(path.join(root, `f-${String(i).padStart(6, '0')}`), 'wx'),
      );
  } else if (profile === 'wide') {
    for (let d = 0; d < 1000; d++) {
      const dir = path.join(root, `d-${String(d).padStart(4, '0')}`);
      fs.mkdirSync(dir);
      for (let f = 0; f < 100; f++)
        fs.closeSync(fs.openSync(path.join(dir, `f-${f}`), 'wx'));
    }
  } else if (profile === 'deep') {
    let dir = root;
    for (let i = 0; i < 256; i++) {
      dir = path.join(dir, 'd');
      fs.mkdirSync(dir);
    }
  } else {
    for (let d = 0; d < 200; d++) fs.mkdirSync(path.join(root, `d-${d}`));
    for (let i = 0; i < 5000; i++) {
      const a = path.join(root, `d-${Math.floor(i / 50)}`, `f-${i}`);
      const b = path.join(root, `d-${Math.floor(i / 50) + 100}`, `f-${i}`);
      fs.writeFileSync(a, 'benchmark shared', { flag: 'wx' });
      fs.linkSync(a, b);
    }
  }
  const seen = new Set();
  let logical = 0n,
    allocated = 0n,
    entries = 0n;
  const walk = (file, isRoot = false) => {
    const stat = fs.lstatSync(file, { bigint: true });
    if (!isRoot) entries++;
    const key = `${stat.dev}:${stat.ino}`;
    if (!seen.has(key)) {
      seen.add(key);
      logical += stat.size;
      allocated += stat.blocks * 512n;
    }
    if (stat.isDirectory())
      for (const name of fs.readdirSync(file)) walk(path.join(file, name));
  };
  walk(root, true);
  if (entries.toString() !== profiles[profile].entries)
    throw new Error('FIXTURE_COUNT_MISMATCH');
  console.log(
    JSON.stringify({
      selectionPath: root,
      expectation: {
        profile,
        logical: logical.toString(),
        allocated: allocated.toString(),
      },
      entries: entries.toString(),
    }),
  );
} catch (error) {
  fs.rmSync(root, { recursive: true });
  throw error;
}
