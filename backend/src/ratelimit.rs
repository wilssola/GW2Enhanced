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
/// Per network (IPv4 /24, IPv6 /48) on top of the per-IP limits: one IPv6 user owns millions of addresses, so a
/// per-address bucket alone is no limit. Roomy enough for many players behind one carrier NAT.
pub const CONTRIBUTIONS_NET: Limit = Limit {
    name: "contrib-net",
    capacity: 10_000.0,
    per_sec: 3.0,
};
pub const VOTES_NET: Limit = Limit {
    name: "vote-net",
    capacity: 120.0,
    per_sec: 0.4,
};
pub const CORRECTIONS_NET: Limit = Limit {
    name: "correct-net",
    capacity: 30.0,
    per_sec: 1.0 / 20.0,
};
/// Downloads counted per network: 10, then one per hour (extra downloads still redirect, they are just not counted).
pub const DOWNLOADS: Limit = Limit {
    name: "dl-net",
    capacity: 10.0,
    per_sec: 1.0 / 3600.0,
};
/// Read endpoints (`/v1/texts`), per IP in the isolate's memory (no D1 write per read): 120 tokens in a burst, then
/// 2 per second. A request costs [`read_cost`] tokens.
pub const READS: Limit = Limit {
    name: "read",
    capacity: 120.0,
    per_sec: 2.0,
};

/// Tokens of [`READS`] one list request takes: a search scans every text (about 3 rows read per text), and a deep
/// page walks `offset` index entries first (about 2 rows read each), so both cost more than a first page.
pub fn read_cost(search: bool, offset: u32) -> f64 {
    let base = if search { 5.0 } else { 1.0 };
    (base + (offset / 5_000) as f64).min(READS.capacity)
}

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
    /// State to store when allowed. A refusal stores nothing: the stored state refills to the same tokens later
    /// (refill is linear and capped), so a flood of refused requests costs no writes.
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

/// Buckets known to be empty, per isolate: until their retry time a refused caller is answered without reading D1.
/// Bounded like [`MemoryBuckets`].
#[derive(Default)]
pub struct Refusals {
    until_ms: std::collections::HashMap<String, i64>,
}

impl Refusals {
    pub const MAX: usize = 4096;

    /// Seconds left of a remembered refusal of `key`.
    pub fn retry_after(&self, key: &str, now_ms: i64) -> Option<u64> {
        let until = *self.until_ms.get(key)?;
        (until > now_ms).then(|| ((until - now_ms) as u64).div_ceil(1000).max(1))
    }

    pub fn remember(&mut self, key: &str, now_ms: i64, retry_after: u64) {
        if self.until_ms.len() >= Self::MAX && !self.until_ms.contains_key(key) {
            self.until_ms.retain(|_, u| *u > now_ms);
            if self.until_ms.len() >= Self::MAX {
                self.until_ms.clear();
            }
        }
        self.until_ms.insert(key.to_string(), now_ms + retry_after as i64 * 1000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_storing_a_refusal_loses_no_refill() {
        // Stored after an allowed take at t=0 with 0 tokens left; refused at t=500 ms (not stored).
        let stored = Bucket { tokens: 0.0, updated_ms: 0 };
        assert!(!take(&L, Some(stored), 500, 1.0).allowed);
        // At t=1 s the token is there, exactly as if the refusal had been stored.
        let refused_state = take(&L, Some(stored), 500, 1.0).bucket;
        assert_eq!(take(&L, Some(stored), 1_000, 1.0), take(&L, Some(refused_state), 1_000, 1.0));
        assert!(take(&L, Some(stored), 1_000, 1.0).allowed);
    }

    #[test]
    fn refusals_expire_and_stay_bounded() {
        let mut r = Refusals::default();
        assert_eq!(r.retry_after("a", 0), None);
        r.remember("a", 0, 10);
        assert_eq!(r.retry_after("a", 0), Some(10));
        assert_eq!(r.retry_after("a", 9_500), Some(1));
        assert_eq!(r.retry_after("a", 10_000), None);
        for i in 0..Refusals::MAX + 10 {
            r.remember(&i.to_string(), 0, 5);
        }
        assert!(r.until_ms.len() <= Refusals::MAX);
    }

    #[test]
    fn read_costs() {
        assert_eq!(read_cost(false, 0), 1.0);
        assert_eq!(read_cost(false, 4_999), 1.0);
        assert_eq!(read_cost(false, 200_000), 41.0);
        assert_eq!(read_cost(true, 0), 5.0);
        // A deep page of the real dictionary (~230k texts) still fits the burst: slow, never impossible.
        assert!(read_cost(false, 500_000) <= READS.capacity);
        assert_eq!(read_cost(false, 2_000_000), READS.capacity);
    }

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
