//! Retention: which stored backups may be deleted.
//!
//! Timestamps come from the object *name*, never from mtime: SFTP and Dropbox
//! do not report reliable mtimes, and a copied backup would otherwise look new.
//! The job's filename template is the only thing that maps a name back to a
//! time, so only objects matching that pattern are ever considered.

use chrono::{DateTime, Utc};
use opendal::Operator;

use crate::config::JobConfig;
use crate::error::Result;
use crate::storage;

/// A stored backup with its parsed timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// Object path relative to the backend root, including the job prefix.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Timestamp encoded in the object name.
    pub timestamp: DateTime<Utc>,
}

impl Backup {
    /// Whether this backup is older than `now - retention_days`.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>, retention_days: u32) -> bool {
        let cutoff = now - chrono::Duration::days(i64::from(retention_days));
        self.timestamp < cutoff
    }
}

/// What a prune run decided to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// Objects that will be (or were) deleted.
    pub expired: Vec<Backup>,
    /// Objects kept because `min_keep` protects them.
    pub protected_by_min_keep: Vec<Backup>,
    /// Files under the prefix that are not this job's backups.
    pub ignored: Vec<String>,
}

impl PrunePlan {
    /// Number of objects the plan deletes.
    #[must_use]
    pub fn delete_count(&self) -> usize {
        self.expired.len()
    }

    /// Human readable summary, one line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} expired, {} protected by min_keep, {} ignored (not matching the filename pattern)",
            self.expired.len(),
            self.protected_by_min_keep.len(),
            self.ignored.len(),
        )
    }
}

/// List this job's backups under its prefix, newest first.
///
/// Objects whose name does not match the job's filename pattern are reported as
/// ignored rather than returned, so callers cannot accidentally act on them.
///
/// # Errors
///
/// [`Error::Storage`] when the prefix cannot be listed.
pub async fn list_backups(op: &Operator, job: &JobConfig) -> Result<(Vec<Backup>, Vec<String>)> {
    let backend = job.storage.kind();
    let prefix = job.storage.prefix().trim_matches('/');
    let listed = storage::list_prefix(op, backend, prefix).await?;

    let mut backups = Vec::new();
    let mut ignored = Vec::new();
    for object in listed {
        match job.parse_object_name(&object.path) {
            Some(timestamp) => backups.push(Backup {
                path: object.path,
                size: object.size,
                timestamp,
            }),
            None => ignored.push(object.path),
        }
    }

    // Newest first, with the object path as a tiebreaker so the order is total.
    backups.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| a.path.cmp(&b.path))
    });
    ignored.sort();

    Ok((backups, ignored))
}

/// Decide what to delete, without touching storage.
///
/// `retention_days` and `min_keep` come from the job. The newest `min_keep`
/// backups are always kept, and the newest backup overall is never deleted, so a
/// single misconfigured day cannot wipe out everything.
#[must_use]
pub fn plan_prune(job: &JobConfig, backups: &[Backup], now: DateTime<Utc>) -> PrunePlan {
    let mut plan = PrunePlan::default();

    // `backups` is newest first, so the protected slice is at the front.
    let protect_from = std::cmp::min(job.min_keep as usize, backups.len());
    let (protected, candidates) = backups.split_at(protect_from);
    plan.protected_by_min_keep = protected.to_vec();

    for backup in candidates {
        if backup.is_expired(now, job.retention_days) {
            plan.expired.push(backup.clone());
        }
    }

    plan
}

/// List, plan and (unless `dry_run`) delete expired backups.
///
/// # Errors
///
/// [`Error::Storage`] when listing fails, or when a delete fails. A failed
/// delete is not skipped: the object stays and the next run retries it.
pub async fn prune(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
    dry_run: bool,
) -> Result<PrunePlan> {
    let backend = job.storage.kind();
    let (backups, ignored) = list_backups(op, job).await?;
    let mut plan = plan_prune(job, &backups, now);

    for path in &ignored {
        tracing::debug!(path, "leaving non-matching object alone");
    }
    plan.ignored = ignored;

    if dry_run {
        for backup in &plan.expired {
            tracing::info!(path = %backup.path, age_days = age_days(backup, now), "would delete");
        }
        return Ok(plan);
    }

    for backup in &plan.expired {
        tracing::info!(
            path = %backup.path,
            bytes = backup.size,
            age_days = age_days(backup, now),
            "deleting expired backup"
        );
        // A delete that fails must not be silently skipped: the object stays and
        // the next run tries again.
        storage::delete_object(op, backend, &backup.path).await?;
    }

    Ok(plan)
}

/// Whole days between a backup and `now`, never negative.
fn age_days(backup: &Backup, now: DateTime<Utc>) -> i64 {
    (now - backup.timestamp).num_days().max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn backup(name: &str, timestamp: DateTime<Utc>, size: u64) -> Backup {
        Backup {
            path: format!("db/{name}"),
            size,
            timestamp,
        }
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .single()
            .expect("valid time")
    }

    fn job(retention_days: u32, min_keep: u32) -> JobConfig {
        use crate::testutil::JobBuilder;
        JobBuilder::new("db")
            .retention(retention_days)
            .min_keep(min_keep)
            .build()
    }

    /// Put `bytes` at `path` in the memory backend.
    async fn put(op: &Operator, path: &str, bytes: &[u8]) {
        crate::storage::write_bytes(op, "memory", path, bytes.to_vec())
            .await
            .expect("write");
    }

    async fn keys(op: &Operator) -> Vec<String> {
        let mut keys: Vec<String> = crate::storage::list_prefix(op, "memory", "db")
            .await
            .expect("list")
            .into_iter()
            .map(|object| object.path)
            .collect();
        keys.sort();
        keys
    }

    fn memory_operator() -> Operator {
        Operator::new(opendal::services::Memory::default()).expect("memory operator")
    }

    #[test]
    fn boundary_exactly_at_retention_is_kept() {
        let now = at(2024, 3, 10, 0, 0, 0);
        let backup = backup("db-20240303T000000Z.tar.zst", at(2024, 3, 3, 0, 0, 0), 1);
        // 7 days old with a 7 day policy: not yet expired.
        assert!(!backup.is_expired(now, 7));
    }

    #[test]
    fn one_second_past_the_boundary_is_expired() {
        let now = at(2024, 3, 10, 0, 0, 0);
        let backup = backup("db-20240302T235959Z.tar.zst", at(2024, 3, 2, 23, 59, 59), 1);
        assert!(backup.is_expired(now, 7));
    }

    #[test]
    fn the_boundary_compares_full_precision_not_whole_days() {
        let now = at(2024, 3, 10, 12, 0, 0);

        // Exactly at the boundary: age == retention_days, which is not *greater
        // than* the window, so the backup is kept.
        let at_boundary = backup("db-20240303T120000Z.tar.zst", at(2024, 3, 3, 12, 0, 0), 1);
        assert!(!at_boundary.is_expired(now, 7));

        // One second older tips it over.
        let one_second_past = backup("db-20240303T115959Z.tar.zst", at(2024, 3, 3, 11, 59, 59), 1);
        assert!(one_second_past.is_expired(now, 7));
    }

    #[test]
    fn min_keep_protects_the_newest_backups() {
        let now = at(2024, 3, 10, 0, 0, 0);
        let job = job(1, 3);
        let backups = vec![
            backup("db-20240309T000000Z.tar.zst", at(2024, 3, 9, 0, 0, 0), 10),
            backup("db-20240308T000000Z.tar.zst", at(2024, 3, 8, 0, 0, 0), 10),
            backup("db-20240307T000000Z.tar.zst", at(2024, 3, 7, 0, 0, 0), 10),
            backup("db-20240301T000000Z.tar.zst", at(2024, 3, 1, 0, 0, 0), 10),
        ];

        let plan = plan_prune(&job, &backups, now);
        assert_eq!(plan.protected_by_min_keep.len(), 3);
        assert_eq!(plan.expired.len(), 1);
        assert_eq!(plan.expired[0].timestamp, at(2024, 3, 1, 0, 0, 0));
    }

    #[test]
    fn the_newest_backup_is_never_deleted() {
        let now = at(2030, 1, 1, 0, 0, 0);
        let job = job(1, 1);
        // A single, very old backup.
        let backups = vec![backup(
            "db-20240101T000000Z.tar.zst",
            at(2024, 1, 1, 0, 0, 0),
            10,
        )];

        let plan = plan_prune(&job, &backups, now);
        assert!(
            plan.expired.is_empty(),
            "the only backup must survive: {:?}",
            plan.expired
        );
        assert_eq!(plan.protected_by_min_keep.len(), 1);
    }

    #[test]
    fn min_keep_larger_than_the_backup_count_keeps_everything() {
        let now = at(2030, 1, 1, 0, 0, 0);
        let job = job(1, 10);
        let backups = vec![
            backup("db-20240101T000000Z.tar.zst", at(2024, 1, 1, 0, 0, 0), 10),
            backup("db-20240102T000000Z.tar.zst", at(2024, 1, 2, 0, 0, 0), 10),
        ];

        let plan = plan_prune(&job, &backups, now);
        assert_eq!(plan.expired, Vec::new());
        assert_eq!(plan.protected_by_min_keep.len(), 2);
    }

    #[test]
    fn nothing_expires_when_everything_is_fresh() {
        let now = at(2024, 3, 10, 0, 0, 0);
        let job = job(30, 1);
        let backups = vec![
            backup("db-20240309T000000Z.tar.zst", at(2024, 3, 9, 0, 0, 0), 10),
            backup("db-20240308T000000Z.tar.zst", at(2024, 3, 8, 0, 0, 0), 10),
        ];

        let plan = plan_prune(&job, &backups, now);
        assert_eq!(plan.expired, Vec::new());
    }

    #[test]
    fn age_days_is_never_negative() {
        let now = at(2024, 3, 10, 0, 0, 0);
        let future = backup("db-20240311T000000Z.tar.zst", at(2024, 3, 11, 0, 0, 0), 1);
        assert_eq!(age_days(&future, now), 0);
    }

    #[test]
    fn plan_summary_mentions_all_three_groups() {
        let plan = PrunePlan {
            expired: vec![backup("a", at(2024, 1, 1, 0, 0, 0), 1)],
            protected_by_min_keep: vec![backup("b", at(2024, 3, 9, 0, 0, 0), 1)],
            ignored: vec!["c".to_owned()],
        };
        let summary = plan.summary();
        assert!(summary.contains("1 expired"), "{summary}");
        assert!(summary.contains("1 protected"), "{summary}");
        assert!(summary.contains("1 ignored"), "{summary}");
        assert_eq!(plan.delete_count(), 1);
    }

    // --- list_backups / prune against the memory backend ---

    #[tokio::test]
    async fn list_backups_parses_timestamps_from_names() {
        let op = memory_operator();
        let job = job(7, 3);
        put(&op, "db/db-20240301T010203Z.tar.zst", b"a").await;
        put(&op, "db/db-20240309T010203Z.tar.zst", b"b").await;

        let (backups, ignored) = list_backups(&op, &job).await.expect("list");
        assert_eq!(ignored, Vec::<String>::new());
        // Newest first.
        assert_eq!(backups.len(), 2);
        assert_eq!(backups[0].timestamp, at(2024, 3, 9, 1, 2, 3));
        assert_eq!(backups[0].path, "db/db-20240309T010203Z.tar.zst");
        assert_eq!(backups[0].size, 1);
        assert_eq!(backups[1].timestamp, at(2024, 3, 1, 1, 2, 3));
    }

    #[tokio::test]
    async fn list_backups_ignores_objects_that_do_not_match_the_pattern() {
        let op = memory_operator();
        let job = job(7, 3);
        put(&op, "db/db-20240309T010203Z.tar.zst", b"a").await;
        put(&op, "db/some-other-file.tar.zst", b"x").await;
        put(&op, "db/db-not-a-timestamp.tar.zst", b"y").await;
        put(&op, "db/notes.txt", b"z").await;

        let (backups, ignored) = list_backups(&op, &job).await.expect("list");
        assert_eq!(backups.len(), 1);
        assert_eq!(
            ignored,
            vec![
                "db/db-not-a-timestamp.tar.zst".to_owned(),
                "db/notes.txt".to_owned(),
                "db/some-other-file.tar.zst".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn list_backups_ignores_objects_outside_the_prefix() {
        let op = memory_operator();
        let job = job(7, 3);
        put(&op, "db/db-20240309T010203Z.tar.zst", b"a").await;
        put(&op, "other/db-20240101T000000Z.tar.zst", b"b").await;

        let (backups, ignored) = list_backups(&op, &job).await.expect("list");
        assert_eq!(backups.len(), 1);
        assert!(
            ignored.is_empty(),
            "objects outside the prefix must not appear: {ignored:?}"
        );
    }

    #[tokio::test]
    async fn prune_deletes_only_expired_backups() {
        let op = memory_operator();
        let job = job(7, 1);
        put(&op, "db/db-20240301T000000Z.tar.zst", b"old").await;
        put(&op, "db/db-20240305T000000Z.tar.zst", b"mid").await;
        put(&op, "db/db-20240309T000000Z.tar.zst", b"new").await;

        let now = at(2024, 3, 10, 0, 0, 0);
        let plan = prune(&op, &job, now, false).await.expect("prune");
        // min_keep=1 protects 03-09; 03-01 is past the 7 day window; 03-05 is
        // still fresh and stays.
        assert_eq!(plan.delete_count(), 1);
        assert_eq!(plan.protected_by_min_keep.len(), 1);

        assert_eq!(
            keys(&op).await,
            vec![
                "db/db-20240305T000000Z.tar.zst".to_owned(),
                "db/db-20240309T000000Z.tar.zst".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn prune_respects_min_keep_against_real_objects() {
        let op = memory_operator();
        let job = job(1, 3);
        put(&op, "db/db-20240309T000000Z.tar.zst", b"a").await;
        put(&op, "db/db-20240308T000000Z.tar.zst", b"b").await;
        put(&op, "db/db-20240307T000000Z.tar.zst", b"c").await;
        put(&op, "db/db-20240101T000000Z.tar.zst", b"d").await;

        let now = at(2024, 3, 10, 0, 0, 0);
        let plan = prune(&op, &job, now, false).await.expect("prune");
        assert_eq!(plan.delete_count(), 1);

        let remaining = keys(&op).await;
        assert_eq!(
            remaining.len(),
            3,
            "min_keep=3 protects the newest three: {remaining:?}"
        );
        assert!(!remaining.contains(&"db/db-20240101T000000Z.tar.zst".to_owned()));
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let op = memory_operator();
        let job = job(7, 1);
        put(&op, "db/db-20240101T000000Z.tar.zst", b"a").await;
        put(&op, "db/db-20240309T000000Z.tar.zst", b"b").await;

        let now = at(2024, 3, 10, 0, 0, 0);
        let plan = prune(&op, &job, now, true).await.expect("dry run");
        assert_eq!(plan.delete_count(), 1);
        assert_eq!(
            keys(&op).await,
            vec![
                "db/db-20240101T000000Z.tar.zst".to_owned(),
                "db/db-20240309T000000Z.tar.zst".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn prune_never_deletes_unrelated_files() {
        let op = memory_operator();
        let job = job(1, 1);
        put(&op, "db/db-20240309T000000Z.tar.zst", b"a").await;
        put(&op, "db/db-20240101T000000Z.tar.zst", b"b").await;
        put(&op, "db/important-notes.txt", b"keep me").await;
        put(&op, "db/manual-copy.tar.zst", b"keep me too").await;

        let now = at(2030, 1, 1, 0, 0, 0);
        prune(&op, &job, now, false).await.expect("prune");

        let remaining = keys(&op).await;
        assert!(remaining.contains(&"db/important-notes.txt".to_owned()));
        assert!(remaining.contains(&"db/manual-copy.tar.zst".to_owned()));
        assert!(remaining.contains(&"db/db-20240309T000000Z.tar.zst".to_owned()));
        assert!(!remaining.contains(&"db/db-20240101T000000Z.tar.zst".to_owned()));
    }

    #[tokio::test]
    async fn prune_on_empty_storage_is_a_no_op() {
        let op = memory_operator();
        let job = job(1, 1);
        let plan = prune(&op, &job, at(2030, 1, 1, 0, 0, 0), false)
            .await
            .expect("prune");
        assert_eq!(plan.delete_count(), 0);
        assert_eq!(keys(&op).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_prune_run_is_idempotent() {
        let op = memory_operator();
        let job = job(7, 1);
        put(&op, "db/db-20240101T000000Z.tar.zst", b"a").await;
        put(&op, "db/db-20240309T000000Z.tar.zst", b"b").await;

        let now = at(2024, 3, 10, 0, 0, 0);
        assert_eq!(
            prune(&op, &job, now, false)
                .await
                .expect("first")
                .delete_count(),
            1
        );
        assert_eq!(
            prune(&op, &job, now, false)
                .await
                .expect("second")
                .delete_count(),
            0
        );
    }
}
