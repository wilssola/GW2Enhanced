//! Token bucket math for the per-IP-hash rate limits (state lives in D1).

/// A bucket kind: holds up to `capacity` tokens, refilled at `per_sec`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    pub name: &'static str,
    pub capacity: f64,
    pub per_sec: f64,
}

/// Contributions cost one token per item: 3000 items in a burst, then about
/// 1 item per second (86k per day) per IP.
pub const CONTRIBUTIONS: Limit = Limit {
    name: "contrib",
    capacity: 3000.0,
    per_sec: 1.0,
};
/// One token per vote: 30 in a burst, then one every 10 s.
pub const VOTES: Limit = Limit {
    name: "vote",
    capacity: 30.0,
    per_sec: 0.1,
};
/// One token per correction: 10 in a burst, then one per minute.
pub const CORRECTIONS: Limit = Limit {
    name: "correct",
    capacity: 10.0,
    per_sec: 1.0 / 60.0,
};
/// Downloads counted per IP: 3, then one every 8 hours (extra downloads still
/// redirect, they are just not counted).
pub const DOWNLOADS: Limit = Limit {
    name: "dl",
    capacity: 3.0,
    per_sec: 1.0 / (8.0 * 3600.0),
};
/// Read endpoints (`/v1/texts`): 120 in a burst, then 2 per second.
pub const READS: Limit = Limit {
    name: "read",
    capacity: 120.0,
    per_sec: 2.0,
};

/// Stored bucket state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub tokens: f64,
    pub updated_ms: i64,
}

/// Result of trying to take `cost` tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    /// State to store (also stored when refused, so refills are not lost).
    pub bucket: Bucket,
    /// Seconds until `cost` tokens are available (0 when allowed).
    pub retry_after: u64,
}

/// Refills the bucket for the time elapsed since its last update, then takes
/// `cost` tokens if there are enough. A missing bucket starts full. A cost
/// above the capacity can never be served and is refused.
pub fn take(limit: &Limit, state: Option<Bucket>, now_ms: i64, cost: f64) -> Decision {
    let tokens = match state {
        None => limit.capacity,
        Some(b) => {
            let elapsed = (now_ms - b.updated_ms).max(0) as f64 / 1000.0;
            (b.tokens + elapsed * limit.per_sec).min(limit.capacity)
        }
    };
    if cost <= tokens {
        return Decision {
            allowed: true,
            bucket: Bucket {
                tokens: tokens - cost,
                updated_ms: now_ms,
            },
            retry_after: 0,
        };
    }
    let missing = cost.min(limit.capacity) - tokens;
    let retry_after = if cost > limit.capacity {
        3600
    } else {
        (missing / limit.per_sec).ceil().max(1.0) as u64
    };
    Decision {
        allowed: false,
        bucket: Bucket {
            tokens,
            updated_ms: now_ms,
        },
        retry_after,
    }
}

/// Buckets kept in the memory of one isolate: the fallback when the D1 bucket cannot be read or stored, so a limit
/// still holds (per isolate) while D1 is out of its daily quota. Bounded: past [`MemoryBuckets::MAX`] keys it starts over.
#[derive(Default)]
pub struct MemoryBuckets {
    buckets: std::collections::HashMap<String, Bucket>,
}

impl MemoryBuckets {
    pub const MAX: usize = 4096;

    pub fn take(&mut self, key: &str, limit: &Limit, now_ms: i64, cost: f64) -> Decision {
        if self.buckets.len() >= Self::MAX && !self.buckets.contains_key(key) {
            self.buckets.clear();
        }
        let d = take(limit, self.buckets.get(key).copied(), now_ms, cost);
        self.buckets.insert(key.to_string(), d.bucket);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_buckets_limit_per_key_and_stay_bounded() {
        let lim = Limit { name: "t", capacity: 2.0, per_sec: 0.0 };
        let mut m = MemoryBuckets::default();
        assert!(m.take("a", &lim, 0, 1.0).allowed);
        assert!(m.take("a", &lim, 0, 1.0).allowed);
        assert!(!m.take("a", &lim, 0, 1.0).allowed);
        assert!(m.take("b", &lim, 0, 1.0).allowed);
        for i in 0..MemoryBuckets::MAX + 10 {
            m.take(&i.to_string(), &lim, 0, 1.0);
        }
        assert!(m.buckets.len() <= MemoryBuckets::MAX);
    }

    const L: Limit = Limit {
        name: "t",
        capacity: 10.0,
        per_sec: 1.0,
    };

    #[test]
    fn starts_full_and_drains() {
        let d = take(&L, None, 0, 10.0);
        assert!(d.allowed);
        assert_eq!(d.bucket.tokens, 0.0);
        let d = take(&L, Some(d.bucket), 0, 1.0);
        assert!(!d.allowed);
        assert_eq!(d.retry_after, 1);
    }

    #[test]
    fn refills_over_time_up_to_capacity() {
        let b = Bucket {
            tokens: 0.0,
            updated_ms: 0,
        };
        let d = take(&L, Some(b), 5_000, 5.0);
        assert!(d.allowed);
        assert_eq!(d.bucket.tokens, 0.0);
        let d = take(&L, Some(b), 1_000_000, 0.0);
        assert_eq!(d.bucket.tokens, 10.0);
    }

    #[test]
    fn retry_after_and_oversized_cost() {
        let b = Bucket {
            tokens: 2.0,
            updated_ms: 0,
        };
        let d = take(&L, Some(b), 0, 7.0);
        assert!(!d.allowed);
        assert_eq!(d.retry_after, 5);
        assert!(!take(&L, None, 0, 11.0).allowed);
        // Clock going backwards does not mint tokens.
        let d = take(
            &L,
            Some(Bucket {
                tokens: 1.0,
                updated_ms: 10_000,
            }),
            0,
            2.0,
        );
        assert!(!d.allowed);
    }

    #[test]
    fn contribution_limit_fits_a_full_request() {
        assert!(take(&CONTRIBUTIONS, None, 0, 500.0).allowed);
    }
}
