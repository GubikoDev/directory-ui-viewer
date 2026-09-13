# Internal app measurement

This is a verification aid, not final product UI or an acceptance certificate.
Use only freshly generated test folders, through the normal native picker.

```sh
node e2e/prepare-native-fixture.mjs hardlinks
VITE_INTERNAL_BENCHMARK=1 npm run tauri dev
```

For a standalone local instrumented app:

```sh
VITE_INTERNAL_BENCHMARK=1 npm run tauri -- build --debug --bundles app --no-sign --config '{"bundle":{"active":true}}'
```

Choose the generator's `selectionPath` in the picker and paste only its
`expectation` object into the measurement panel. The generator accepts exactly
one fixed profile: `flat10k`, `flat100k`, `wide`, `deep`, or `hardlinks`. It creates
a fresh folder in the OS temporary directory and independently sums `lstat`
metadata, including the root, with `(dev, ino)` deduplication. Directory entry
count excludes the root and includes every hardlink name. Keep that manifest
locally outside Git; it contains a temporary path. Remove only those owned
fixtures when verification finishes.

Each run attempts ten trials using the existing SessionClient and adapter. A
trial refreshes the approved root, waits for the first requested page and two
animation frames with matching rendered generation, then observes terminal
usage and compares its bytes/count with the oracle. Timing starts **before**
refresh, including the default directory listing and scan activation. File
profiles then request their file listing. A separate refresh prepares the
cancellation observation. This includes IPC and React observation overhead;
it is not engine-only timing or proof of exact physical screen paint timing.

First-page success requires 200 entries within 1,000 ms. The single-child deep
profile cannot satisfy a 200-row page, so its first-page result is `null`.
Deep traversal must report resource-limit partial measures and a nonzero count
below the full tree size. Other profiles require exact complete measures and
counts. `alreadyTerminal` cancellation is reported with a `null` latency pass;
it never proves cancellation of running work. The recorded pre-request phase
is a frontend observation and may lag native completion. Accepted cancellation
requires at most 100 ms for the latency comparison; separately establish that
work was running before claiming the running-cancellation acceptance gate.

Check `layer`, `stoppedBy`, trial count, and **all** comparisons. `fixture`
results, fewer than ten trials, mismatches, or `RESOURCE_LIMIT` do not pass the
actual-app gate. Repeated refreshes may hit the managed limit while older
observations await GC; the harness neither raises that limit nor forces GC.
In a jsdom integration experiment, 200-directory repeated refreshes stopped at
that limit. Actual WebKit behavior must still be measured. The report preserves
completed trials on failure and excludes filenames, paths, and session IDs.

“측정 중지 요청” stops subsequent steps after the current request returns.
It does not interrupt an OS call or automatically cancel the background scan.
The panel remains occupied while that request drains, preventing overlapping
runs. The normal connection-close control remains available. The 120-second
polling deadline is not a timeout for an already pending native request.

`frontendUsed` is managed cost at the usage observation. High-water cost includes
cancellation and is cumulative since document start, not reset for each trial.
These counters include reservations and retained observations; they are not
process RSS. Native managed peaks, process RSS, FD peaks, whole-owner cost
audits, and actual WebKit security/picker/lifecycle checks need separate evidence.
Record build mode, hardware, OS, filesystem, source revision, and OS-cache
conditions with each real run. OS caches are not flushed by this tool. A normal
build without `VITE_INTERNAL_BENCHMARK=1` omits the panel and measurement code.

## External process metrics on macOS

```sh
cargo build --manifest-path src-tauri/Cargo.toml --example app_resources
src-tauri/target/debug/examples/app_resources 60000 10 APP_PID WEBKIT_PID
```

Replace the PID placeholders with the **verified** app and associated WebKit
process IDs. The observer accepts 1–8 explicit PIDs and runs for at most ten
minutes. It does not discover helpers, infer ownership from process names, or
claim that the supplied set covers the whole app. Verify each process's role
before interpreting app totals. Shared WebKit/GPU processes also need careful
attribution. Record that mapping with the local run evidence, not filenames or
directory contents. Use a local ignored JSONL file for stdout.

The observer uses the installed SDK's public `proc_pid_rusage` v4 and
`proc_pidinfo(PROC_PIDLISTFDS)` interfaces. Each sample includes resident bytes,
physical footprint, the kernel's lifetime maximum physical footprint, and FD
count. Lifetime physical footprint is **not** an RSS peak and includes activity
before the observation window. RSS and FD maxima are only the maxima of samples;
shorter peaks between samples can be missed. Per-process actual sample gaps are
reported, including delay caused by observation and output. Different PIDs and
the RSS/FD syscalls are sampled sequentially, not atomically. A sum of individual
process maxima is neither a simultaneous peak nor a deduplicated physical total.

The start identity is checked around FD sampling and on subsequent samples;
process exit/reuse, permission failures, or a full fixed 8,192-entry FD buffer
stop the run with a nonzero exit and `stoppedBy`. They never become zero-valued
success samples. A live process with no readable FD list is conservatively
reported unavailable. Partial samples remain in stdout. Output failures or
external interruption may leave no summary; that is an incomplete run.

No target memory is read and no target files are opened. The observer itself
does not run in the app's managed memory budget. Its self-tests allocate/touch
owned memory and create anonymous pipes; its CLI tests launch only owned
temporary processes:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --example app_resources -- --test-threads=1 --nocapture
node e2e/process-resources.mjs
```

These verify the observer, not actual app resource acceptance. Native exact
managed high-water counters and actual WebKit/app runs are still separate work.
