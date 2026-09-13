//! External, read-only process sampling for an explicitly supplied PID set.
//! Does not enumerate paths, grant filesystem access, or include unnamed WebKit
//! processes automatically. RSS/FD peaks are sampled; footprint lifetime peak
//! is a different, kernel-reported metric. See e2e/MEASUREMENT.md.

#[cfg(target_os = "macos")]
mod mac {
    use serde::Serialize;
    use std::{
        io::Write,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Failure {
        code: &'static str,
        errno: Option<i32>,
    }
    impl Failure {
        fn code(code: &'static str) -> Self {
            Self { code, errno: None }
        }
        fn os(code: &'static str) -> Self {
            Self {
                code,
                errno: std::io::Error::last_os_error().raw_os_error(),
            }
        }
    }
    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Sample {
        pid: i32,
        process_start_abstime: u64,
        resident_bytes: u64,
        physical_footprint_bytes: u64,
        lifetime_max_physical_footprint_bytes: u64,
        file_descriptors: usize,
    }
    struct Reader {
        // A fixed observer-side buffer. A full buffer is a failed sample, never
        // a truncated count reported as an exact process FD observation.
        descriptors: [libc::proc_fdinfo; 8192],
    }
    impl Reader {
        fn new() -> Self {
            Self {
                descriptors: unsafe { std::mem::zeroed() },
            }
        }
        fn usage(pid: i32) -> Result<libc::rusage_info_v4, Failure> {
            let mut value: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::proc_pid_rusage(
                    pid,
                    libc::RUSAGE_INFO_V4,
                    (&mut value as *mut libc::rusage_info_v4).cast(),
                )
            };
            if result != 0 {
                return Err(Failure::os("RUSAGE_UNAVAILABLE"));
            }
            if value.ri_proc_start_abstime == 0 || value.ri_proc_exit_abstime != 0 {
                return Err(Failure::code("PROCESS_NOT_LIVE"));
            }
            Ok(value)
        }
        fn sample(&mut self, pid: i32, identity: Option<u64>) -> Result<Sample, Failure> {
            if pid <= 0 {
                return Err(Failure::code("INVALID_PID"));
            }
            let before = Self::usage(pid)?;
            if identity.is_some_and(|id| id != before.ri_proc_start_abstime) {
                return Err(Failure::code("PROCESS_IDENTITY_CHANGED"));
            }
            let capacity = std::mem::size_of_val(&self.descriptors);
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    self.descriptors.as_mut_ptr().cast(),
                    capacity as i32,
                )
            };
            if bytes <= 0 {
                return Err(Failure::os("FD_SAMPLE_UNAVAILABLE"));
            }
            if bytes as usize >= capacity {
                return Err(Failure::code("FD_SAMPLE_TRUNCATED"));
            }
            let element = std::mem::size_of::<libc::proc_fdinfo>();
            if !(bytes as usize).is_multiple_of(element) {
                return Err(Failure::code("FD_SAMPLE_INVALID"));
            }
            // Reject exit or PID reuse between the two independent OS reads.
            let after = Self::usage(pid)?;
            if before.ri_proc_start_abstime != after.ri_proc_start_abstime {
                return Err(Failure::code("PROCESS_IDENTITY_CHANGED"));
            }
            Ok(Sample {
                pid,
                process_start_abstime: after.ri_proc_start_abstime,
                resident_bytes: after.ri_resident_size,
                physical_footprint_bytes: after.ri_phys_footprint,
                lifetime_max_physical_footprint_bytes: after.ri_lifetime_max_phys_footprint,
                file_descriptors: bytes as usize / element,
            })
        }
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Peak {
        pid: i32,
        process_start_abstime: u64,
        samples: u64,
        last_sample_elapsed_us: u128,
        max_sample_gap_us: u128,
        resident_max_sampled_bytes: u64,
        physical_footprint_max_sampled_bytes: u64,
        lifetime_max_physical_footprint_bytes: u64,
        fd_max_sampled: usize,
    }
    impl Peak {
        fn new(sample: Sample, elapsed_us: u128) -> Self {
            let mut peak = Self {
                pid: sample.pid,
                process_start_abstime: sample.process_start_abstime,
                samples: 0,
                last_sample_elapsed_us: elapsed_us,
                max_sample_gap_us: 0,
                resident_max_sampled_bytes: 0,
                physical_footprint_max_sampled_bytes: 0,
                lifetime_max_physical_footprint_bytes: 0,
                fd_max_sampled: 0,
            };
            peak.observe(sample, elapsed_us);
            peak
        }
        fn observe(&mut self, sample: Sample, elapsed_us: u128) {
            self.max_sample_gap_us = self
                .max_sample_gap_us
                .max(elapsed_us - self.last_sample_elapsed_us);
            self.last_sample_elapsed_us = elapsed_us;
            self.samples += 1;
            self.resident_max_sampled_bytes =
                self.resident_max_sampled_bytes.max(sample.resident_bytes);
            self.physical_footprint_max_sampled_bytes = self
                .physical_footprint_max_sampled_bytes
                .max(sample.physical_footprint_bytes);
            self.lifetime_max_physical_footprint_bytes = self
                .lifetime_max_physical_footprint_bytes
                .max(sample.lifetime_max_physical_footprint_bytes);
            self.fd_max_sampled = self.fd_max_sampled.max(sample.file_descriptors);
        }
    }
    fn emit(value: impl Serialize) -> Result<(), Failure> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &value).map_err(|_| Failure::code("OUTPUT_FAILED"))?;
        out.write_all(b"\n")
            .and_then(|_| out.flush())
            .map_err(|_| Failure::code("OUTPUT_FAILED"))
    }
    fn parse(args: &[String]) -> Result<(u64, u64, Vec<i32>), Failure> {
        if !(3..=10).contains(&args.len()) {
            return Err(Failure::code("INVALID_ARGUMENT"));
        }
        let duration = args[0]
            .parse::<u64>()
            .map_err(|_| Failure::code("INVALID_ARGUMENT"))?;
        let interval = args[1]
            .parse::<u64>()
            .map_err(|_| Failure::code("INVALID_ARGUMENT"))?;
        if !(1..=600_000).contains(&duration)
            || !(1..=1000).contains(&interval)
            || interval > duration
        {
            return Err(Failure::code("INVALID_ARGUMENT"));
        }
        let mut pids = Vec::with_capacity(args.len() - 2);
        for text in &args[2..] {
            let pid = text
                .parse::<i32>()
                .map_err(|_| Failure::code("INVALID_PID"))?;
            if pid <= 0 || pids.contains(&pid) {
                return Err(Failure::code("INVALID_PID"));
            }
            pids.push(pid);
        }
        Ok((duration, interval, pids))
    }
    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let (duration, interval, pids) = parse(&args).map_err(|_| {
            "Usage: app_resources DURATION_MS INTERVAL_MS PID [PID...] (1–8 explicit PIDs, <=600000ms)".to_owned()
        })?;
        let started = Instant::now();
        let mut reader = Reader::new();
        let mut peaks = Vec::<Peak>::with_capacity(pids.len());
        let mut max_sample_gap_us = 0;
        let mut last_tick = started;
        let mut stopped_by = None;
        emit(
            serde_json::json!({"type":"start", "durationMs":duration, "intervalMs":interval,
            "unixMs":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
            "pids":pids, "scope":"explicit PID set only; no process-tree or WebKit discovery"}),
        )
        .map_err(|e| e.code.to_owned())?;
        'sampling: loop {
            let now = Instant::now();
            max_sample_gap_us = max_sample_gap_us.max(now.duration_since(last_tick).as_micros());
            last_tick = now;
            for (index, &pid) in pids.iter().enumerate() {
                let identity = peaks.get(index).map(|peak| peak.process_start_abstime);
                match reader.sample(pid, identity) {
                    Ok(sample) => {
                        let elapsed_us = started.elapsed().as_micros();
                        if let Some(peak) = peaks.get_mut(index) {
                            peak.observe(sample, elapsed_us);
                        } else {
                            peaks.push(Peak::new(sample, elapsed_us));
                        }
                        emit(serde_json::json!({"type":"sample","elapsedUs":elapsed_us,"value":sample}))
                            .map_err(|e| e.code.to_owned())?;
                    }
                    Err(error) => {
                        stopped_by = Some(serde_json::json!({"pid":pid,"error":error}));
                        break 'sampling;
                    }
                }
            }
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(duration) {
                break;
            }
            std::thread::sleep(
                Duration::from_millis(interval).min(Duration::from_millis(duration) - elapsed),
            );
        }
        let failed = stopped_by.is_some();
        emit(
            serde_json::json!({"type":"summary","elapsedUs":started.elapsed().as_micros(),
            "maxTickGapUs":max_sample_gap_us,"stoppedBy":stopped_by,"peaks":peaks}),
        )
        .map_err(|e| e.code.to_owned())?;
        if failed {
            Err("Sampling stopped before a complete observation window; inspect stoppedBy.".into())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::{FromRawFd, OwnedFd};

        #[test]
        fn observes_touched_memory_and_live_descriptors_without_paths() {
            let mut reader = Reader::new();
            let pid = std::process::id() as i32;
            let baseline = reader.sample(pid, None).unwrap();
            let mut pipes = Vec::<OwnedFd>::with_capacity(32);
            for _ in 0..16 {
                let mut fds = [-1; 2];
                assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
                pipes.extend(fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
            }
            let mut memory = vec![0_u8; 64 * 1024 * 1024];
            for chunk in memory.chunks_mut(4096) {
                chunk[0] = 1;
            }
            std::hint::black_box(&memory);
            let loaded = reader
                .sample(pid, Some(baseline.process_start_abstime))
                .unwrap();
            assert_eq!(loaded.file_descriptors, baseline.file_descriptors + 32);
            assert!(loaded.resident_bytes >= baseline.resident_bytes + 32 * 1024 * 1024);
            assert!(
                loaded.lifetime_max_physical_footprint_bytes >= loaded.physical_footprint_bytes
            );
            drop(pipes);
            let closed = reader
                .sample(pid, Some(baseline.process_start_abstime))
                .unwrap();
            assert_eq!(closed.file_descriptors, baseline.file_descriptors);
            assert_eq!(
                reader
                    .sample(pid, Some(baseline.process_start_abstime + 1))
                    .unwrap_err()
                    .code,
                "PROCESS_IDENTITY_CHANGED"
            );
            println!(
                "{}",
                serde_json::json!({"baseline":baseline,"loaded":loaded,"closed":closed})
            );
        }
        #[test]
        fn rejects_invalid_and_duplicate_pids() {
            for args in [
                vec!["10", "1", "0"],
                vec!["10", "1", "-1"],
                vec!["10", "1", "1", "1"],
                vec!["600001", "1", "1"],
            ] {
                assert!(parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
            }
            assert!(parse(&["20", "10", "1"].map(String::from)).is_ok());
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = mac::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("app_resources requires macOS");
    std::process::exit(1);
}
