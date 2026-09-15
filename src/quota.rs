use anyhow::{Result, bail};
use std::{
    collections::VecDeque,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Leave headroom below Gmail's published 6,000 units/user/project/minute.
const MINUTE_BUDGET: u32 = 4_500;
const BURST_BUDGET: u32 = 800;

#[derive(Default)]
struct Budget {
    requests: VecDeque<(Instant, u32)>,
    blocked_until: Option<Instant>,
    last_rejection: Option<Instant>,
    rejections: u32,
}

impl Budget {
    fn reserve(&mut self, now: Instant, units: u32) -> Result<Duration> {
        if let Some(until) = self.blocked_until.filter(|until| *until > now) {
            return Ok(until - now);
        }
        while self
            .requests
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_secs(60))
        {
            self.requests.pop_front();
        }
        let mut wait = Duration::ZERO;
        let limit = MINUTE_BUDGET / (1 << self.rejections.min(3));
        for (window, limit) in [(60, limit), (1, BURST_BUDGET)] {
            let recent = self
                .requests
                .iter()
                .filter(|(at, _)| now.duration_since(*at) < Duration::from_secs(window));
            let mut total = recent.clone().map(|(_, cost)| cost).sum::<u32>() + units;
            for (at, cost) in recent {
                if total <= limit {
                    break;
                }
                wait = wait.max((*at + Duration::from_secs(window)).duration_since(now));
                total -= cost;
            }
        }
        if wait.is_zero() {
            self.requests.push_back((now, units));
        }
        Ok(wait)
    }

    fn reject(&mut self, now: Instant, retry_after: Duration) {
        if self
            .last_rejection
            .is_none_or(|last| now.duration_since(last) > Duration::from_secs(600))
        {
            self.rejections = 0;
        }
        // Concurrent failures from the same burst must not multiply the backoff.
        if self.blocked_until.is_none_or(|until| until <= now) {
            self.rejections = (self.rejections + 1).min(4);
        }
        self.last_rejection = Some(now);
        let delay =
            Duration::from_secs(60 * (1 << self.rejections.saturating_sub(1))).max(retry_after);
        let until = now + delay;
        self.blocked_until = Some(self.blocked_until.map_or(until, |old| old.max(until)));
    }
}

// Only request timestamps/costs and cooldowns are persisted, never mail or tokens.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct SavedBudget {
    requests: Vec<(u64, u32)>,
    blocked_until: Option<u64>,
    last_rejection: Option<u64>,
    rejections: u32,
}

impl SavedBudget {
    fn restore(self, now: Instant, wall: u64) -> Budget {
        Budget {
            requests: self
                .requests
                .into_iter()
                .filter(|(at, _)| wall.saturating_sub(*at) < 60_000)
                .map(|(at, units)| (now - Duration::from_millis(wall.saturating_sub(at)), units))
                .collect(),
            blocked_until: self
                .blocked_until
                .filter(|until| *until > wall)
                .map(|until| now + Duration::from_millis(until - wall)),
            last_rejection: self
                .last_rejection
                .filter(|at| wall.saturating_sub(*at) < 600_000)
                .map(|at| now - Duration::from_millis(wall.saturating_sub(at))),
            rejections: if self
                .last_rejection
                .is_some_and(|at| wall.saturating_sub(at) < 600_000)
            {
                self.rejections
            } else {
                0
            },
        }
    }

    fn snapshot(budget: &Budget, now: Instant, wall: u64) -> Self {
        let past =
            |at: Instant| wall.saturating_sub(now.saturating_duration_since(at).as_millis() as u64);
        Self {
            requests: budget
                .requests
                .iter()
                .map(|(at, units)| (past(*at), *units))
                .collect(),
            blocked_until: budget
                .blocked_until
                .filter(|until| *until > now)
                .map(|until| wall + until.duration_since(now).as_millis() as u64),
            last_rejection: budget.last_rejection.map(past),
            rejections: budget.rejections,
        }
    }
}

fn database_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "Postbird", "Postbird")
        .ok_or_else(|| anyhow::anyhow!("Could not locate Postbird's data directory"))?;
    std::fs::create_dir_all(dirs.data_local_dir())?;
    Ok(dirs.data_local_dir().join("quota.db"))
}

fn with_budget<T>(
    account: &str,
    operation: impl FnOnce(&mut Budget, Instant) -> Result<T>,
) -> Result<T> {
    with_budget_at(&database_path()?, account, operation)
}

fn with_budget_at<T>(
    path: &Path,
    account: &str,
    operation: impl FnOnce(&mut Budget, Instant) -> Result<T>,
) -> Result<T> {
    use rusqlite::OptionalExtension;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let mut db = rusqlite::Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS quota (account TEXT PRIMARY KEY, state TEXT NOT NULL)",
    )?;
    // Serialize reservations across workers AND overlapping/restarted app instances.
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let account = account.to_ascii_lowercase();
    let saved: Option<String> = tx
        .query_row(
            "SELECT state FROM quota WHERE account = ?1",
            [&account],
            |row| row.get(0),
        )
        .optional()?;
    let now = Instant::now();
    let wall = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let mut budget = saved
        .as_ref()
        .map(|json| serde_json::from_str::<SavedBudget>(json))
        .transpose()?
        .unwrap_or_default()
        .restore(now, wall);
    let result = operation(&mut budget, now)?;
    let json = serde_json::to_string(&SavedBudget::snapshot(&budget, now, wall))?;
    if saved.as_deref() != Some(json.as_str()) {
        tx.execute("INSERT INTO quota (account, state) VALUES (?1, ?2) ON CONFLICT(account) DO UPDATE SET state = excluded.state", rusqlite::params![account, json])?;
    }
    tx.commit()?;
    Ok(result)
}

pub fn acquire(account: &str, units: u32, cancelled: Option<&AtomicBool>) -> Result<()> {
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            bail!("Request cancelled");
        }
        let delay = with_budget(account, |budget, now| budget.reserve(now, units))?;
        if delay.is_zero() {
            return Ok(());
        }
        // No mutex held while waiting; newer mailbox loads can cancel this work.
        std::thread::sleep(delay.min(Duration::from_millis(100)));
    }
}

pub fn reject(account: &str, retry_after: Duration) -> Result<()> {
    with_budget(account, |budget, now| {
        budget.reject(now, retry_after);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_refreshes_share_a_rolling_budget() {
        let start = Instant::now();
        let mut budget = Budget::default();
        for second in 0..6 {
            for _ in 0..18 {
                assert!(
                    budget
                        .reserve(start + Duration::from_secs(second), 40)
                        .unwrap()
                        .is_zero()
                );
            }
        }
        let now = start + Duration::from_secs(6);
        assert!(budget.reserve(now, 100).unwrap().is_zero());
        assert!(budget.reserve(now, 100).unwrap() >= Duration::from_secs(54));
        assert!(
            budget
                .reserve(start + Duration::from_secs(60), 100)
                .unwrap()
                .is_zero()
        );
    }
    #[test]
    fn burst_limits_and_cooldowns_do_not_charge_waiting_requests() {
        let now = Instant::now();
        let mut budget = Budget::default();
        for _ in 0..20 {
            assert!(budget.reserve(now, 40).unwrap().is_zero());
        }
        assert_eq!(budget.reserve(now, 40).unwrap(), Duration::from_secs(1));
        assert_eq!(budget.requests.len(), 20);
        budget.reject(now, Duration::from_secs(90));
        budget.reject(now, Duration::ZERO);
        assert_eq!(budget.rejections, 1);
        assert!(
            budget.reserve(now + Duration::from_secs(89), 1).unwrap() == Duration::from_secs(1)
        );
        budget.reject(now + Duration::from_secs(90), Duration::ZERO);
        assert_eq!(budget.rejections, 2);
        assert!(
            budget.reserve(now + Duration::from_secs(209), 1).unwrap() == Duration::from_secs(1)
        );
        assert!(
            budget
                .reserve(now + Duration::from_secs(210), 1)
                .unwrap()
                .is_zero()
        );
    }
    #[test]
    fn quota_and_cooldowns_survive_reopening_the_database() {
        let dir = std::env::temp_dir().join(format!(
            "postbird-quota-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("quota.db");
        with_budget_at(&path, "Test@Example.com", |budget, now| {
            for _ in 0..112 {
                budget
                    .requests
                    .push_back((now - Duration::from_secs(10), 40));
            }
            Ok(())
        })
        .unwrap();
        let delay = with_budget_at(&path, "test@example.com", |budget, now| {
            budget.reserve(now, 40)
        })
        .unwrap();
        assert!(delay > Duration::from_secs(45));
        assert!(
            with_budget_at(&path, "other@example.com", |budget, now| budget
                .reserve(now, 40))
            .unwrap()
            .is_zero()
        );
        with_budget_at(&path, "test@example.com", |budget, now| {
            budget.reject(now, Duration::from_secs(120));
            Ok(())
        })
        .unwrap();
        let delay = with_budget_at(&path, "test@example.com", |budget, now| {
            budget.reserve(now, 1)
        })
        .unwrap();
        assert!(delay > Duration::from_secs(115));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejection_lowers_the_budget_and_quiet_period_restores_it() {
        let now = Instant::now();
        let mut budget = Budget::default();
        budget.reject(now, Duration::ZERO);
        for _ in 0..56 {
            budget
                .requests
                .push_back((now + Duration::from_secs(65), 40));
        }
        assert!(budget.reserve(now + Duration::from_secs(70), 40).unwrap() > Duration::ZERO);
        let saved = SavedBudget::snapshot(&budget, now + Duration::from_secs(70), 70_000);
        let restored = saved.restore(now + Duration::from_secs(700), 700_000);
        assert_eq!(restored.rejections, 0);
        assert!(restored.requests.is_empty());
    }

    #[test]
    fn cancelled_load_does_not_use_quota() {
        assert!(acquire("cancel-test", 40, Some(&AtomicBool::new(true))).is_err());
    }
}
