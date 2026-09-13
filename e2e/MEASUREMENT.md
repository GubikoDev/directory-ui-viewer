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
