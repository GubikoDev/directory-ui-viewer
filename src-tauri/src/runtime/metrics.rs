//! Opt-in native managed counters. No IPC command, path, identifier or authority.
//! Current components are sequential reads. Per-pool lifetime peaks are exact
//! counters; their sum is an upper bound, not a simultaneous aggregate peak.
use super::memory::{ByteBudget, ManagedBudgets, Reservation, ResourceLimit};
use crate::scheduler::HandleBudget;
use serde::Serialize;
use std::{
    io::{self, Write},
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const RECORDER_BYTES: usize = 4096;
const LINE_BYTES: usize = 2048;
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Stage {
    Startup,
    ConnectBegin,
    ConnectEnd,
    RequestBegin,
    RequestEnd,
    DocumentBegin,
    DocumentEnd,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pool {
    used: usize,
    lifetime_peak: usize,
    limit: usize,
}
impl Pool {
    fn read(budget: &ByteBudget) -> Self {
        Self {
            used: budget.used(),
            lifetime_peak: budget.peak(),
            limit: budget.limit(),
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    kind: &'static str,
    stage: Stage,
    ordinal: u64,
    span_begin_ordinal: Option<u64>,
    pid: u32,
    elapsed_us: u128,
    unix_ms: u128,
    background_index: Pool,
    foreground_index: Pool,
    listings: Pool,
    hardlinks: Pool,
    work: Pool,
    scan_workspace: Pool,
    replay: Pool,
    component_lifetime_peak_sum_upper_bound: usize,
    managed_fd_current: usize,
    managed_fd_lifetime_peak: usize,
    recorder_reserved_bytes: usize,
}
struct Output {
    bytes: [u8; LINE_BYTES],
    ordinal: u64,
}
pub(crate) struct Recorder {
    budgets: ManagedBudgets,
    handles: HandleBudget,
    started: Instant,
    output: Mutex<Output>,
    _memory: Reservation,
}
struct Buffer<'a> {
    bytes: &'a mut [u8],
    len: usize,
}
impl Write for Buffer<'_> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let end = self
            .len
            .checked_add(input.len())
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::WriteZero))?;
        self.bytes[self.len..end].copy_from_slice(input);
        self.len = end;
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Recorder {
    pub(crate) fn new(
        budgets: ManagedBudgets,
        handles: HandleBudget,
    ) -> Result<Self, ResourceLimit> {
        let memory = budgets.work.reserve(RECORDER_BYTES)?;
        Ok(Self {
            budgets,
            handles,
            started: Instant::now(),
            output: Mutex::new(Output {
                bytes: [0; LINE_BYTES],
                ordinal: 0,
            }),
            _memory: memory,
        })
    }
    fn snapshot(&self, stage: Stage, ordinal: u64) -> Snapshot {
        let background_index = Pool::read(&self.budgets.background_index);
        let foreground_index = Pool::read(&self.budgets.foreground_index);
        let listings = Pool::read(&self.budgets.listings);
        let hardlinks = Pool::read(&self.budgets.hardlinks);
        let scan_workspace = Pool::read(&self.budgets.scan_workspace);
        let replay = Pool::read(&self.budgets.replay);
        let work = Pool::read(&self.budgets.work);
        Snapshot {
            kind: "nativeManagedCounters",
            stage,
            ordinal,
            span_begin_ordinal: None,
            pid: std::process::id(),
            elapsed_us: self.started.elapsed().as_micros(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            background_index,
            foreground_index,
            listings,
            hardlinks,
            work,
            scan_workspace,
            replay,
            component_lifetime_peak_sum_upper_bound: background_index.lifetime_peak
                + foreground_index.lifetime_peak
                + listings.lifetime_peak
                + hardlinks.lifetime_peak
                + scan_workspace.lifetime_peak
                + replay.lifetime_peak
                + work.lifetime_peak,
            managed_fd_current: self.handles.open(),
            managed_fd_lifetime_peak: self.handles.peak(),
            recorder_reserved_bytes: RECORDER_BYTES,
        }
    }
    fn render(snapshot: &Snapshot, bytes: &mut [u8]) -> io::Result<usize> {
        let mut buffer = Buffer { bytes, len: 0 };
        serde_json::to_writer(&mut buffer, snapshot)?;
        buffer.write_all(b"\n")?;
        Ok(buffer.len)
    }
    pub(crate) fn emit(&self, stage: Stage) -> u64 {
        self.emit_inner(stage, None)
    }
    fn emit_inner(&self, stage: Stage, begin: Option<u64>) -> u64 {
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(ordinal) = output.ordinal.checked_add(1) else {
            return 0;
        };
        output.ordinal = ordinal;
        // Errors lose this record, never affect command authorization/results.
        // Ordinal gaps or a missing expected end marker make a trace incomplete.
        let mut snapshot = self.snapshot(stage, ordinal);
        snapshot.span_begin_ordinal = begin;
        if let Ok(len) = Self::render(&snapshot, &mut output.bytes) {
            let _ = io::stderr().lock().write_all(&output.bytes[..len]);
        }
        ordinal
    }
    pub(crate) fn span(&self, begin: Stage, end: Stage) -> Span<'_> {
        let begin = self.emit(begin);
        Span {
            recorder: self,
            begin,
            end,
        }
    }
}
pub(crate) struct Span<'a> {
    recorder: &'a Recorder,
    begin: u64,
    end: Stage,
}
impl Drop for Span<'_> {
    fn drop(&mut self) {
        self.recorder.emit_inner(self.end, Some(self.begin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_shared_current_and_lifetime_counters_and_keeps_recorder_charged() {
        let budgets = ManagedBudgets::default();
        let handles = HandleBudget::new(320);
        let recorder = Recorder::new(budgets.clone(), handles.clone()).unwrap();
        assert_eq!(budgets.work.used(), RECORDER_BYTES);
        let allocation = budgets.foreground_index.reserve(1234).unwrap();
        let handle = handles.acquire().unwrap();
        let first = recorder.snapshot(Stage::RequestBegin, 1);
        assert_eq!(first.foreground_index.used, 1234);
        assert_eq!(first.managed_fd_current, 1);
        drop((allocation, handle));
        let last = recorder.snapshot(Stage::RequestEnd, 2);
        assert_eq!(last.foreground_index.used, 0);
        assert_eq!(last.foreground_index.lifetime_peak, 1234);
        assert_eq!(last.managed_fd_current, 0);
        assert_eq!(last.managed_fd_lifetime_peak, 1);
        assert_eq!(
            last.component_lifetime_peak_sum_upper_bound,
            1234 + RECORDER_BYTES
        );
        drop(recorder);
        assert_eq!(budgets.work.used(), 0);
    }
    #[test]
    fn maximum_numeric_fields_fit_the_fixed_line_and_smaller_output_fails() {
        let recorder = Recorder::new(ManagedBudgets::default(), HandleBudget::new(320)).unwrap();
        let mut value = recorder.snapshot(Stage::DocumentBegin, u64::MAX);
        let max = Pool {
            used: usize::MAX,
            lifetime_peak: usize::MAX,
            limit: usize::MAX,
        };
        value.background_index = max;
        value.foreground_index = max;
        value.listings = max;
        value.hardlinks = max;
        value.work = max;
        value.scan_workspace = max;
        value.replay = max;
        value.elapsed_us = u128::MAX;
        value.unix_ms = u128::MAX;
        value.component_lifetime_peak_sum_upper_bound = usize::MAX;
        value.managed_fd_current = usize::MAX;
        value.managed_fd_lifetime_peak = usize::MAX;
        value.recorder_reserved_bytes = usize::MAX;
        value.span_begin_ordinal = Some(u64::MAX);
        let mut bytes = [0; LINE_BYTES];
        let len = Recorder::render(&value, &mut bytes).unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&bytes[..len]).unwrap();
        assert_eq!(decoded["kind"], "nativeManagedCounters");
        assert!(len < LINE_BYTES);
        assert!(Recorder::render(&value, &mut [0; 10]).is_err());
    }
    #[test]
    fn overlapping_spans_keep_their_matching_begin_when_owners_drop() {
        let recorder = Recorder::new(ManagedBudgets::default(), HandleBudget::new(320)).unwrap();
        let first = recorder.span(Stage::RequestBegin, Stage::RequestEnd);
        let second = recorder.span(Stage::RequestBegin, Stage::RequestEnd);
        assert_eq!(recorder.output.lock().unwrap().ordinal, 2);
        let latest = || {
            let output = recorder.output.lock().unwrap();
            let end = output.bytes.iter().position(|&byte| byte == b'\n').unwrap();
            serde_json::from_slice::<serde_json::Value>(&output.bytes[..end]).unwrap()
        };
        drop(first);
        assert_eq!(latest()["ordinal"], 3);
        assert_eq!(latest()["spanBeginOrdinal"], 1);
        drop(second);
        assert_eq!(latest()["ordinal"], 4);
        assert_eq!(latest()["spanBeginOrdinal"], 2);
    }
}
