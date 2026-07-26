use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

const IMPORTANT_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(30),
];
const IDLE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];
const IMPORTANT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostPhase {
    Cached,
    Connecting,
    Online,
    Backoff,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureClass {
    Transient,
    Blocked,
}

#[derive(Clone, Debug)]
pub(crate) struct HostReconnectState {
    phase: HostPhase,
    consecutive_failures: usize,
    next_attempt_at: Option<Instant>,
}

impl HostReconnectState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            phase: HostPhase::Cached,
            consecutive_failures: 0,
            next_attempt_at: Some(now),
        }
    }

    pub(crate) const fn phase(&self) -> HostPhase {
        self.phase
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.next_attempt_at.is_some_and(|due| now >= due)
    }

    pub(crate) fn begin_attempt(&mut self) {
        self.phase = HostPhase::Connecting;
        self.next_attempt_at = None;
    }

    pub(crate) fn request_now(&mut self, now: Instant) {
        self.phase = HostPhase::Cached;
        self.consecutive_failures = 0;
        self.next_attempt_at = Some(now);
    }

    pub(crate) fn succeeded(&mut self, now: Instant, important: bool) -> bool {
        let recovered = self.consecutive_failures > 0;
        self.phase = HostPhase::Online;
        self.consecutive_failures = 0;
        self.next_attempt_at = Some(
            now + if important {
                IMPORTANT_REFRESH_INTERVAL
            } else {
                IDLE_REFRESH_INTERVAL
            },
        );
        recovered
    }

    pub(crate) fn failed(
        &mut self,
        now: Instant,
        target: &str,
        important: bool,
        jitter_seed: u64,
        class: FailureClass,
    ) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        match class {
            FailureClass::Blocked => {
                self.phase = HostPhase::Blocked;
                self.next_attempt_at = None;
            }
            FailureClass::Transient => {
                self.phase = HostPhase::Backoff;
                self.next_attempt_at = Some(
                    now + retry_delay(target, self.consecutive_failures, important, jitter_seed),
                );
            }
        }
    }

    pub(crate) fn retry_in(&self, now: Instant) -> Option<Duration> {
        self.next_attempt_at
            .map(|due| due.saturating_duration_since(now))
    }
}

pub(crate) fn classify_failure(message: &str) -> FailureClass {
    let message = message.to_ascii_lowercase();
    if [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "no such identity",
        "authentication failed",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
    {
        FailureClass::Blocked
    } else {
        FailureClass::Transient
    }
}

fn retry_delay(target: &str, failure_count: usize, important: bool, jitter_seed: u64) -> Duration {
    let delays = if important {
        &IMPORTANT_RETRY_DELAYS
    } else {
        &IDLE_RETRY_DELAYS
    };
    let base = delays[failure_count.saturating_sub(1).min(delays.len() - 1)];
    jitter(base, target, failure_count, jitter_seed)
}

fn jitter(base: Duration, target: &str, failure_count: usize, jitter_seed: u64) -> Duration {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    failure_count.hash(&mut hasher);
    jitter_seed.hash(&mut hasher);
    let percentage = i64::try_from(hasher.finish() % 41).unwrap_or_default() - 20;
    let base_millis = i128::try_from(base.as_millis()).unwrap_or(i128::MAX);
    let adjusted = base_millis + base_millis * i128::from(percentage) / 100;
    Duration::from_millis(u64::try_from(adjusted.max(1)).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FailureClass, HostPhase, HostReconnectState, classify_failure};

    #[test]
    fn important_hosts_back_off_to_about_thirty_seconds() {
        let now = Instant::now();
        let mut state = HostReconnectState::new(now);
        for failure in 1..=8 {
            state.begin_attempt();
            state.failed(now, "spot", true, 42, FailureClass::Transient);
            let delay = state.retry_in(now).expect("retry should be scheduled");
            if failure >= 5 {
                assert!(delay >= Duration::from_secs(24));
                assert!(delay <= Duration::from_secs(36));
            }
        }
    }

    #[test]
    fn idle_hosts_relax_to_about_two_minutes() {
        let now = Instant::now();
        let mut state = HostReconnectState::new(now);
        for _ in 0..8 {
            state.begin_attempt();
            state.failed(now, "archive", false, 7, FailureClass::Transient);
        }
        let delay = state.retry_in(now).expect("retry should be scheduled");
        assert!(delay >= Duration::from_secs(96));
        assert!(delay <= Duration::from_secs(144));
    }

    #[test]
    fn recovery_is_reported_after_a_failure() {
        let now = Instant::now();
        let mut state = HostReconnectState::new(now);
        state.begin_attempt();
        state.failed(now, "spot", true, 0, FailureClass::Transient);
        assert!(state.succeeded(now, true));
        assert_eq!(state.phase(), HostPhase::Online);
        assert!(!state.succeeded(now, true));
    }

    #[test]
    fn authentication_failures_wait_for_manual_retry() {
        let now = Instant::now();
        let mut state = HostReconnectState::new(now);
        state.begin_attempt();
        state.failed(now, "spot", true, 0, FailureClass::Blocked);
        assert_eq!(state.phase(), HostPhase::Blocked);
        assert_eq!(state.retry_in(now), None);

        state.request_now(now);
        assert!(state.is_due(now));
    }

    #[test]
    fn classifies_security_and_authentication_failures_as_blocked() {
        assert_eq!(
            classify_failure("Permission denied (publickey)."),
            FailureClass::Blocked
        );
        assert_eq!(
            classify_failure("Host key verification failed."),
            FailureClass::Blocked
        );
        assert_eq!(
            classify_failure("ssh: connect to host spot port 22: Connection timed out"),
            FailureClass::Transient
        );
    }
}
