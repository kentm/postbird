use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const RECHECK_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision(u64, u64);

#[derive(Default)]
struct Mailbox {
    revision: u64,
    checked: Option<(Revision, Instant)>,
    message_count: usize,
}

/// Freshness is deliberately session-local: after a restart, reconcile once.
/// An event received during a refresh must not be cleared by that old refresh.
#[derive(Default)]
pub struct MailboxSync {
    accounts: HashMap<String, u64>,
    mailboxes: HashMap<(String, String), Mailbox>,
}

impl MailboxSync {
    pub fn revision(&self, email: &str, label: &str) -> Revision {
        Revision(
            self.accounts.get(email).copied().unwrap_or(0),
            self.mailboxes
                .get(&(email.into(), label.into()))
                .map_or(0, |m| m.revision),
        )
    }

    pub fn is_fresh(&self, email: &str, label: &str, now: Instant) -> bool {
        self.mailboxes
            .get(&(email.into(), label.into()))
            .and_then(|m| m.checked)
            .is_some_and(|(revision, checked)| {
                revision == self.revision(email, label)
                    && now.saturating_duration_since(checked) < RECHECK_AFTER
            })
    }

    pub fn matches_cache(&self, email: &str, label: &str, message_count: usize) -> bool {
        self.mailboxes
            .get(&(email.into(), label.into()))
            .is_some_and(|mailbox| {
                mailbox.checked.is_some() && mailbox.message_count == message_count
            })
    }

    pub fn complete(
        &mut self,
        email: &str,
        label: &str,
        revision: Revision,
        now: Instant,
        message_count: usize,
    ) {
        if revision == self.revision(email, label) {
            self.accounts.entry(email.into()).or_default();
            let mailbox = self
                .mailboxes
                .entry((email.into(), label.into()))
                .or_default();
            mailbox.checked = Some((revision, now));
            mailbox.message_count = message_count;
        }
    }

    pub fn invalidate_account(&mut self, email: &str) {
        *self.accounts.entry(email.into()).or_default() += 1;
    }

    pub fn invalidate_mailbox(&mut self, email: &str, label: &str) {
        self.mailboxes
            .entry((email.into(), label.into()))
            .or_default()
            .revision += 1;
    }

    pub fn retain_accounts(&mut self, emails: &[String]) {
        // Keep revision tombstones so an old in-flight result cannot certify
        // the cache of a removed account that is subsequently added again.
        for (email, revision) in &mut self.accounts {
            if !emails.contains(email) {
                *revision += 1;
            }
        }
        self.mailboxes
            .retain(|(email, _), _| emails.contains(email));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_accounts_reuses_only_successfully_checked_mailboxes() {
        let now = Instant::now();
        let mut sync = MailboxSync::default();
        assert!(!sync.is_fresh("work", "INBOX", now));
        sync.complete("work", "INBOX", sync.revision("work", "INBOX"), now, 0);
        sync.complete(
            "personal",
            "INBOX",
            sync.revision("personal", "INBOX"),
            now,
            0,
        );
        assert!(sync.is_fresh("work", "INBOX", now));
        assert!(!sync.is_fresh("work", "TRASH", now));
        sync.invalidate_account("personal");
        assert!(sync.is_fresh("work", "INBOX", now));
        assert!(!sync.is_fresh("personal", "INBOX", now));
        assert!(!sync.is_fresh("work", "INBOX", now + RECHECK_AFTER));
    }

    #[test]
    fn changes_during_a_refresh_cannot_be_lost_by_its_completion() {
        let now = Instant::now();
        let mut sync = MailboxSync::default();
        let original = sync.revision("account", "INBOX");
        sync.invalidate_account("account");
        sync.complete("account", "INBOX", original, now, 0);
        assert!(!sync.is_fresh("account", "INBOX", now));
        let catchup = sync.revision("account", "INBOX");
        sync.complete("account", "INBOX", catchup, now, 0);
        assert!(sync.is_fresh("account", "INBOX", now));
        sync.invalidate_mailbox("account", "INBOX");
        sync.complete("account", "INBOX", catchup, now, 0);
        assert!(!sync.is_fresh("account", "INBOX", now));
    }

    #[test]
    fn readding_an_account_does_not_reuse_its_previous_freshness() {
        let now = Instant::now();
        let mut sync = MailboxSync::default();
        let old = sync.revision("account", "INBOX");
        sync.complete("account", "INBOX", old, now, 0);
        sync.retain_accounts(&[]);
        sync.complete("account", "INBOX", old, now, 0);
        assert!(!sync.is_fresh("account", "INBOX", now));
    }

    #[test]
    fn empty_folders_can_be_reused_but_a_missing_nonempty_cache_cannot() {
        let now = Instant::now();
        let mut sync = MailboxSync::default();
        assert!(!sync.matches_cache("account", "INBOX", 0));
        sync.complete(
            "account",
            "INBOX",
            sync.revision("account", "INBOX"),
            now,
            2,
        );
        assert!(sync.matches_cache("account", "INBOX", 2));
        assert!(!sync.matches_cache("account", "INBOX", 0));
        sync.complete(
            "account",
            "TRASH",
            sync.revision("account", "TRASH"),
            now,
            0,
        );
        assert!(sync.is_fresh("account", "TRASH", now));
        assert!(sync.matches_cache("account", "TRASH", 0));
    }
}
