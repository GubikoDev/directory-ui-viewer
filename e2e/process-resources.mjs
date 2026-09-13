// Run after `cargo build --manifest-path src-tauri/Cargo.toml --example app_resources`.
// Only observes owned temporary child processes; no app/user files are read.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';

if (process.platform !== 'darwin') throw new Error('macOS is required');
const observer = 'src-tauri/target/debug/examples/app_resources';

async function target() {
  const child = spawn(process.execPath, [
    '-e',
    "process.stdin.resume(); process.stdin.on('end', () => process.exit(0)); console.log('ready');",
  ]);
  const done = new Promise((resolve) => child.once('exit', resolve));
  await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.stdout.once('data', resolve);
    child.once('exit', () =>
      reject(new Error('Target exited before readiness')),
    );
  });
  return { child, done };
}

async function observe(pids, onSample = () => {}) {
  const child = spawn(observer, ['150', '10', ...pids.map(String)]);
  const lines = [];
  let pending = '',
    stderr = '';
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (part) => {
    stderr += part;
  });
  child.stdout.on('data', (part) => {
    pending += part;
    let end;
    while ((end = pending.indexOf('\n')) >= 0) {
      const line = JSON.parse(pending.slice(0, end));
      pending = pending.slice(end + 1);
      lines.push(line);
      if (line.type === 'sample') onSample(line);
    }
  });
  const status = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  assert.equal(pending, '');
  return { lines, status, stderr };
}

const a = await target(),
  b = await target();
try {
  const { lines, status, stderr } = await observe([a.child.pid, b.child.pid]);
  assert.equal(status, 0, stderr);
  const summary = lines.at(-1);
  assert.equal(summary.type, 'summary');
  assert.equal(summary.stoppedBy, null);
  assert.equal(summary.peaks.length, 2);
  for (const peak of summary.peaks) {
    const samples = lines.filter(
      (r) => r.type === 'sample' && r.value.pid === peak.pid,
    );
    assert.equal(peak.samples, samples.length);
    assert(samples.length >= 3);
    assert.equal(
      peak.residentMaxSampledBytes,
      Math.max(...samples.map((s) => s.value.residentBytes)),
    );
    assert.equal(
      peak.fdMaxSampled,
      Math.max(...samples.map((s) => s.value.fileDescriptors)),
    );
    assert.equal(
      peak.maxSampleGapUs,
      Math.max(
        ...samples.slice(1).map((s, i) => s.elapsedUs - samples[i].elapsedUs),
      ),
    );
    assert(
      samples.every(
        (s) => s.value.processStartAbstime === peak.processStartAbstime,
      ),
    );
  }
  console.log(
    'Two explicit live PIDs: samples, identities, maxima, and actual gaps match.',
  );
} finally {
  a.child.stdin.end();
  b.child.stdin.end();
  await Promise.all([a.done, b.done]);
}

const c = await target();
try {
  const { lines, status } = await observe([c.child.pid], () =>
    c.child.stdin.end(),
  );
  await c.done;
  assert.equal(status, 1);
  const summary = lines.at(-1);
  assert.equal(summary.type, 'summary');
  assert(summary.stoppedBy);
  assert(summary.peaks[0].samples > 0);
  console.log(
    'Target exit after first sample: nonzero exit and partial evidence preserved.',
  );
} finally {
  c.child.stdin.end();
  await c.done;
}
