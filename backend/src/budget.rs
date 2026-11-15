//! Global daily budgets of D1 rows (pure; the Worker glue only does I/O).
//!
//! Per-IP limits do not bound what many IPs can cost together, so every D1 write the Worker makes on behalf of a
//! client, and every row its public lists read, is also counted against one budget per UTC day, kept in `counters`
//! (`budget:w:YYYY-MM-DD`, `budget:r:YYYY-MM-DD`). Past it, writes answer 503 until midnight UTC.
//!
//! The row counts below follow how D1 bills writes: every row inserted, updated or deleted is one row, and every
//! index entry it touches is one more (a `TEXT PRIMARY KEY` or `UNIQUE` is an index too). Upserts that end in
//! `DO NOTHING` write nothing.

/// Default write budget (rows/day, `WRITE_BUDGET_DAILY`): 45M rows a month at most, under the 50M the Workers Paid
/// plan includes (the daily seed of `src/bin/jobs.rs` writes at most 75k more).
pub const WRITE_BUDGET_DAILY: u64 = 1_500_000;
/// Default read budget of the public lists (rows/day, `READ_BUDGET_DAILY`): 21B a month at most, under the 25B
/// included.
pub const READ_BUDGET_DAILY: u64 = 700_000_000;

/// Who spends a budget. Public requests stop at four fifths of it; the last fifth is kept for the review cron
/// (writes) and for cheap first pages of the list (reads), so an abuser who drains the public part does not also stop
/// reviews or the plain listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Public,
    Reserve,
}

/// The ceiling `tier` may spend up to.
pub fn cap(budget: u64, tier: Tier) -> u64 {
    match tier {
        Tier::Public => budget - budget / 5,
        Tier::Reserve => budget,
    }
}

/// `YYYY-MM-DD` (UTC) of a Unix time in milliseconds.
pub fn day(now_ms: i64) -> String {
    crate::time::rfc3339(now_ms)[..10].to_string()
}

/// `counters` name of the write budget of the day.
pub fn write_counter(now_ms: i64) -> String {
    format!("budget:w:{}", day(now_ms))
}

/// `counters` name of the read budget of the day.
pub fn read_counter(now_ms: i64) -> String {
    format!("budget:r:{}", day(now_ms))
}

/// Seconds until the next UTC midnight (the `Retry-After` of an exhausted budget), at least 1.
pub fn seconds_to_midnight(now_ms: i64) -> u64 {
    let day_ms = 86_400_000;
    let left = day_ms - now_ms.rem_euclid(day_ms);
    (left as u64).div_ceil(1000).max(1)
}

/// Parses a budget variable; empty or invalid means the default, `0` is allowed (closes the endpoint).
pub fn parse_budget(v: &str, default: u64) -> u64 {
    v.trim().replace('_', "").parse().unwrap_or(default)
}

/// Rows written by the budget's own upsert (a new day's row has its primary key entry too).
pub const RESERVE_ROWS: u64 = 2;
/// Rows written by storing one token bucket (row + primary key entry on insert).
pub const BUCKET_ROWS: u64 = 2;

/// Upper bound of the rows written by one contribution request (`queries::contribution`), all items new.
/// Per text: `texts` (row, key, `texts_state`) 3 + `text_reporters` (row, key) 2 + reporters update 1 = 6.
/// Per ID pair: `id_pairs` (row, key, `id_pairs_text`) 3 + reporters 2 + update 1 = 6.
/// Per translation: `translations` (row, key, `translations_text`, `translations_review`) 4 + reporters 2 +
/// update 1 = 7.
/// Per queued item (one per text, pair and translation): `review_queue` (row, `UNIQUE`, `review_queue_next`) 3,
/// plus 3 `sqlite_sequence` updates per request. Plus the IP and network buckets and the budget row.
pub fn contribution_rows(texts: usize, pairs: usize, translations: usize) -> u64 {
    let (t, p, x) = (texts as u64, pairs as u64, translations as u64);
    t * (6 + 3) + p * (6 + 3) + x * (7 + 3) + 3 + 2 * BUCKET_ROWS + RESERVE_ROWS
}

/// Bytes of text stored per budget unit: a contribution is charged its rows plus one unit per this many bytes of
/// text it carries, so a few huge texts cost about as much as many short ones (this bounds storage growth too).
pub const BYTES_PER_UNIT: u64 = 64;

pub fn bytes_units(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(BYTES_PER_UNIT)
}

/// Upper bound of the rows written by one vote: IP and network buckets 4, `votes` (row, `UNIQUE`, `votes_ip`) 3,
/// up/down recount 1, score 1, `current` upsert (row, key, `current_score`, `current_updated`, `current_net`) 5,
/// budget row 2.
pub const VOTE_ROWS: u64 = 2 * BUCKET_ROWS + 3 + 1 + 1 + 5 + RESERVE_ROWS;
/// Upper bound of the rows written by one correction: buckets 4, `translations` 4, reporter 2, reporter count 1,
/// queue 3 + sequence 1, budget row 2.
pub const CORRECTION_ROWS: u64 = 2 * BUCKET_ROWS + 4 + 2 + 1 + 4 + RESERVE_ROWS;
/// Rows written by one counted download: network bucket 2, counter 1, budget row 2.
pub const DOWNLOAD_ROWS: u64 = BUCKET_ROWS + 1 + RESERVE_ROWS;

/// What one isolate knows of the write budget, so a drained budget is refused without asking D1 again.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WriteGate {
    day: String,
    /// Last total D1 returned for `day` (a lower bound: other isolates add to it).
    total: u64,
    /// Smallest cost D1 refused on `day`.
    refused: Option<u64>,
}

impl WriteGate {
    /// Whether `units` is sure to be refused, from what this isolate saw today.
    pub fn refuses(&self, day: &str, units: u64, cap: u64) -> bool {
        self.day == day && (self.total.saturating_add(units) > cap || self.refused.is_some_and(|r| units >= r))
    }

    fn at(&mut self, day: &str) {
        if self.day != day {
            *self = WriteGate {
                day: day.to_string(),
                ..Default::default()
            };
        }
    }

    /// D1 accepted a reservation and returned the new total.
    pub fn accepted(&mut self, day: &str, total: u64) {
        self.at(day);
        self.total = self.total.max(total);
    }

    /// D1 refused a reservation of `units`.
    pub fn refused(&mut self, day: &str, units: u64) {
        self.at(day);
        self.refused = Some(self.refused.map_or(units, |r| r.min(units)));
    }
}

/// Rows read by the public lists in one isolate, added to the day's counter in D1 every [`ReadMeter::FLUSH_ROWS`]
/// rows or [`ReadMeter::FLUSH_MS`] (one write per flush, not per request).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReadMeter {
    day: String,
    /// Total in D1 at the last flush, `None` before the first one of the day.
    total: Option<u64>,
    pending: u64,
    last_flush_ms: i64,
}

impl ReadMeter {
    pub const FLUSH_ROWS: u64 = 200_000;
    pub const FLUSH_MS: i64 = 30_000;

    fn at(&mut self, day: &str) {
        if self.day != day {
            // Rows of the old day still pending go to the new one (they are few, and counting them is safer).
            *self = ReadMeter {
                day: day.to_string(),
                pending: self.pending,
                ..Default::default()
            };
        }
    }

    /// Whether a read may run: `None` when the day's total is not known yet (read it from D1, then [`Self::synced`]).
    pub fn allows(&mut self, day: &str, cap: u64) -> Option<bool> {
        self.at(day);
        self.total.map(|t| t.saturating_add(self.pending) < cap)
    }

    pub fn add(&mut self, day: &str, rows: u64) {
        self.at(day);
        self.pending = self.pending.saturating_add(rows);
    }

    /// Whether the pending rows should be added to D1 now.
    pub fn should_flush(&self, now_ms: i64) -> bool {
        self.pending > 0
            && (self.total.is_none()
                || self.pending >= Self::FLUSH_ROWS
                || now_ms - self.last_flush_ms >= Self::FLUSH_MS)
    }

    /// Rows to add in a flush (then [`Self::flushed`] with what D1 returned).
    pub fn pending(&self) -> u64 {
        self.pending
    }

    /// D1 added `sent` rows and the day's total is now `total`.
    pub fn flushed(&mut self, day: &str, sent: u64, total: u64, now_ms: i64) {
        self.at(day);
        self.pending = self.pending.saturating_sub(sent);
        self.total = Some(total);
        self.last_flush_ms = now_ms;
    }

    /// The day's total read from D1 without adding to it.
    pub fn synced(&mut self, day: &str, total: u64, now_ms: i64) {
        self.flushed(day, 0, total, now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOV15: i64 = 1_763_164_800_000; // 2025-11-15T00:00:00Z

    #[test]
    fn days_and_midnight() {
        assert_eq!(day(NOV15), "2025-11-15");
        assert_eq!(day(NOV15 - 1), "2025-11-14");
        assert_eq!(write_counter(NOV15 + 5), "budget:w:2025-11-15");
        assert_eq!(read_counter(NOV15), "budget:r:2025-11-15");
        assert_eq!(seconds_to_midnight(NOV15), 86_400);
        assert_eq!(seconds_to_midnight(NOV15 + 86_400_000 - 1), 1);
        assert_eq!(seconds_to_midnight(NOV15 + 3_600_000), 82_800);
    }

    #[test]
    fn caps_and_parsing() {
        assert_eq!(cap(1_000, Tier::Public), 800);
        assert_eq!(cap(1_000, Tier::Reserve), 1_000);
        assert_eq!(parse_budget("", 7), 7);
        assert_eq!(parse_budget("junk", 7), 7);
        assert_eq!(parse_budget(" 2_000_000 ", 7), 2_000_000);
        assert_eq!(parse_budget("0", 7), 0);
        // Monthly worst cases stay under what the Paid plan includes (50M writes, 25B reads).
        const { assert!(WRITE_BUDGET_DAILY * 30 + 75_000 * 30 <= 50_000_000) };
        const { assert!(READ_BUDGET_DAILY * 30 < 25_000_000_000) };
    }

    #[test]
    fn row_estimates() {
        // A full request of 500 new items with an ID and a translation each.
        assert_eq!(contribution_rows(500, 500, 500), 500 * 28 + 3 + 4 + 2);
        assert_eq!(contribution_rows(0, 0, 0), 9);
        assert_eq!(VOTE_ROWS, 16);
        assert_eq!(CORRECTION_ROWS, 17);
        assert_eq!(DOWNLOAD_ROWS, 5);
        assert_eq!(bytes_units(0), 0);
        assert_eq!(bytes_units(1), 1);
        assert_eq!(bytes_units(256 * 1024), 4096);
    }

    #[test]
    fn write_gate_refuses_locally_only_what_d1_would() {
        let mut g = WriteGate::default();
        assert!(!g.refuses("d1", 10, 100));
        g.accepted("d1", 95);
        assert!(g.refuses("d1", 10, 100));
        assert!(!g.refuses("d1", 5, 100));
        // A new day forgets.
        assert!(!g.refuses("d2", 10, 100));
        g.refused("d2", 50);
        assert!(g.refuses("d2", 50, 1_000));
        assert!(g.refuses("d2", 80, 1_000));
        assert!(!g.refuses("d2", 49, 1_000));
        g.refused("d2", 20);
        assert!(g.refuses("d2", 20, 1_000));
        // Totals never go back down from a stale answer.
        g.accepted("d2", 10);
        g.accepted("d2", 5);
        assert_eq!(g.total, 10);
    }

    #[test]
    fn read_meter_flushes_and_caps() {
        let mut m = ReadMeter::default();
        assert_eq!(m.allows("d1", 1_000), None);
        m.synced("d1", 900, 0);
        assert_eq!(m.allows("d1", 1_000), Some(true));
        m.add("d1", 50);
        assert!(!m.should_flush(1_000));
        assert!(m.should_flush(ReadMeter::FLUSH_MS));
        m.add("d1", 60);
        assert_eq!(m.allows("d1", 1_000), Some(false));
        let sent = m.pending();
        m.flushed("d1", sent, 2_000, 5_000);
        assert_eq!(m.pending(), 0);
        assert!(!m.should_flush(1_000_000));
        m.add("d1", ReadMeter::FLUSH_ROWS);
        assert!(m.should_flush(5_001));
        // A new day starts unknown, carrying what was pending.
        assert_eq!(m.allows("d2", 1_000), None);
        assert_eq!(m.pending(), ReadMeter::FLUSH_ROWS);
        assert!(m.should_flush(5_002));
    }
}
