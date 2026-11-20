//! Shared test scaffolding for the integration tests.
//!
//! Not every binary uses every helper, so unused items are expected here.

#![allow(dead_code)]

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ghostwriter_lib::engine::{Phase, Progress, Reporter, Sink};

/// A sink that records what would have been typed.
pub struct RecordingSink(pub Arc<Mutex<Vec<char>>>);

impl Sink for RecordingSink {
    fn emit(&mut self, ch: char) -> Result<(), String> {
        self.0.lock().unwrap().push(ch);
        Ok(())
    }
}

/// A sink that always fails, to exercise the error path.
pub struct FailingSink(pub &'static str);

impl Sink for FailingSink {
    fn emit(&mut self, _ch: char) -> Result<(), String> {
        Err(self.0.to_string())
    }
}

/// Collects every progress report and lets a test block until a phase appears.
#[derive(Default)]
pub struct Recorder {
    log: Mutex<Vec<Progress>>,
    cv: Condvar,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn callback(self: &Arc<Self>) -> Reporter {
        let this = self.clone();
        Arc::new(move |p| {
            this.log.lock().unwrap().push(p);
            this.cv.notify_all();
        })
    }

    pub fn last(&self) -> Option<Progress> {
        self.log.lock().unwrap().last().cloned()
    }

    pub fn phases(&self) -> Vec<Phase> {
        self.log.lock().unwrap().iter().map(|p| p.phase).collect()
    }

    /// The full log, in order, so a test can assert on the whole trajectory.
    pub fn all(&self) -> Vec<Progress> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Blocks until the newest report matches `pred`, or the timeout expires.
    pub fn wait_for(&self, timeout: Duration, pred: impl Fn(&Progress) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut log = self.log.lock().unwrap();
        loop {
            if log.last().is_some_and(&pred) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            log = self.cv.wait_timeout(log, left).unwrap().0;
        }
    }

    pub fn wait_for_phase(&self, timeout: Duration, phase: Phase) -> bool {
        self.wait_for(timeout, |p| p.phase == phase)
    }
}

/// A shared buffer a `RecordingSink` writes into.
pub fn shared_buffer() -> Arc<Mutex<Vec<char>>> {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn buffer_string(buffer: &Arc<Mutex<Vec<char>>>) -> String {
    buffer.lock().unwrap().iter().collect()
}
