use std::time::Instant;
#[derive(Clone)]
pub struct TimeSample {
    pub monotonic_ms: u64,
    pub observed_at: String,
}
pub trait Clock: Send + Sync {
    fn sample(&self) -> TimeSample;
}
pub struct SystemClock {
    started: Instant,
}
impl Default for SystemClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}
impl Clock for SystemClock {
    fn sample(&self) -> TimeSample {
        TimeSample {
            monotonic_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            observed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}
