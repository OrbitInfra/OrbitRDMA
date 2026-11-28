use crate::common::{COUNT, WARM};
use std::time::Instant;

// Diagnostic builds only. The end-to-end comparison uses no stage timestamps.
pub struct Stages {
    samples: [Vec<u64>; 3],
}

impl Stages {
    pub fn new() -> Self {
        Self {
            samples: std::array::from_fn(|_| Vec::with_capacity(COUNT)),
        }
    }

    pub fn record(&mut self, i: usize, start: Instant, post: Instant, cq: Instant, end: Instant) {
        if i >= WARM {
            for (samples, interval) in
                self.samples
                    .iter_mut()
                    .zip([post - start, cq - post, end - cq])
            {
                samples.push(interval.as_nanos() as u64);
            }
        }
    }

    pub fn report(self) {
        for (name, mut samples) in ["post", "poll", "reclaim"].into_iter().zip(self.samples) {
            if samples.is_empty() {
                continue;
            }
            assert_eq!(samples.len(), COUNT);
            samples.sort_unstable();
            println!(
                "STAGE {name} {} {}",
                samples[COUNT / 2 - 1],
                samples[COUNT * 99 / 100 - 1]
            );
        }
    }
}
