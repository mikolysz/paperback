//! Tracks background opens, startup restoration, re-parse batches, and their accessibility status.
//! `DocumentManager` owns this state; it contains no widgets or locks.

use std::{
	path::PathBuf,
	time::{Duration, Instant},
};

use patois::t;

use super::document_manager::title_or_filename;

/// A document open whose parse is running in the background. It remains registered until the
/// final result, including through password retries, to prevent concurrent opens of the same path.
/// Closing all documents cancels pending entries; a completion that no longer finds its entry
/// must discard the result.
pub struct PendingParse {
	pub seq: u64,
	pub path: PathBuf,
	pub key: String,
	pub track: bool,
	pub is_restore: bool,
	pub title_override: Option<String>,
	pub initial_caret: Option<i64>,
	pub focus_when_done: bool,
	pub password: String,
	pub password_attempted: bool,
	pub forced_extension: String,
	pub render_tables_inline: bool,
}

/// State of a re-parse batch after recording one result.
#[derive(Debug, PartialEq, Eq)]
pub enum ReparseJobOutcome {
	/// The result belongs to an older batch and must be discarded.
	Superseded,
	/// The current batch still has work in flight.
	BatchInFlight,
	/// The current batch has finished; `failed` tabs keep their old sessions.
	BatchFinished { failed: usize },
}

const REMINDER_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct ParseRegistry {
	pending: Vec<PendingParse>,
	next_seq: u64,
	/// The startup restore's active tab. A user-initiated open (including re-selecting an
	/// already-open tab) clears it so restore cannot override the user's selection.
	restore_active_key: Option<String>,
	/// Identifies the current re-parse batch so results from older batches can be discarded.
	restore_failed: usize,
	reparse_generation: u64,
	reparse_jobs_left: usize,
	reparse_failed: usize,
	/// Start of the current reminder interval, or `None` while idle.
	reminder_anchor: Option<Instant>,
}

impl ParseRegistry {
	/// Registers `entry` with the next sequence number. A user open also cancels the startup
	/// restore's pending tab selection.
	pub fn register(&mut self, mut entry: PendingParse) -> u64 {
		let seq = self.next_seq;
		self.next_seq += 1;
		entry.seq = seq;
		if !entry.is_restore {
			self.restore_active_key = None;
		}
		if self.pending.is_empty() && self.reparse_jobs_left == 0 {
			// A busy period starting between two polls must not inherit the previous one's clock.
			self.reminder_anchor = None;
		}
		self.pending.push(entry);
		seq
	}

	pub fn by_key_mut(&mut self, key: &str) -> Option<&mut PendingParse> {
		self.pending.iter_mut().find(|entry| entry.key == key)
	}

	/// Returns `None` for an entry that was cancelled before its parse completed.
	pub fn by_seq_mut(&mut self, seq: u64) -> Option<&mut PendingParse> {
		self.pending.iter_mut().find(|entry| entry.seq == seq)
	}

	/// Removes `seq`. If it completes startup restore, returns the group's failure count. Returns
	/// `None` for an entry that was cancelled before its parse completed.
	pub fn take(&mut self, seq: u64, failed: bool) -> Option<(PendingParse, Option<usize>)> {
		let index = self.pending.iter().position(|entry| entry.seq == seq)?;
		let entry = self.pending.remove(index);
		if entry.is_restore && failed {
			self.restore_failed += 1;
		}
		let restore_finished = (entry.is_restore && !self.pending.iter().any(|e| e.is_restore))
			.then(|| std::mem::take(&mut self.restore_failed));
		Some((entry, restore_finished))
	}

	/// Cancels every pending open, including any startup restore; their in-flight parse results
	/// are discarded when they complete.
	pub fn cancel_all(&mut self) {
		self.pending.clear();
		self.restore_active_key = None;
		self.restore_failed = 0;
	}

	pub const fn has_pending(&self) -> bool {
		!self.pending.is_empty()
	}

	pub fn set_restore_active_key(&mut self, key: Option<String>) {
		self.restore_active_key = key;
	}

	pub fn restore_active_key(&self) -> Option<&str> {
		self.restore_active_key.as_deref()
	}

	pub fn take_restore_active_key(&mut self) -> Option<String> {
		self.restore_active_key.take()
	}

	/// Starts a re-parse batch and supersedes any older batch still in flight.
	pub const fn begin_reparse_batch(&mut self, jobs: usize) -> u64 {
		if self.pending.is_empty() && self.reparse_jobs_left == 0 {
			// A busy period starting between two polls must not inherit the previous one's clock.
			self.reminder_anchor = None;
		}
		self.reparse_generation += 1;
		self.reparse_jobs_left = jobs;
		self.reparse_failed = 0;
		self.reparse_generation
	}

	/// Records a re-parse result and reports whether its batch is stale, active, or complete.
	pub const fn reparse_job_done(&mut self, generation: u64, failed: bool) -> ReparseJobOutcome {
		if generation != self.reparse_generation {
			return ReparseJobOutcome::Superseded;
		}
		self.reparse_jobs_left -= 1;
		if failed {
			self.reparse_failed += 1;
		}
		if self.reparse_jobs_left == 0 {
			ReparseJobOutcome::BatchFinished { failed: self.reparse_failed }
		} else {
			ReparseJobOutcome::BatchInFlight
		}
	}

	/// Names one pending open, counts several, or describes an active re-parse batch.
	pub fn busy_status_text(&self) -> Option<String> {
		match self.pending.len() {
			// TRANSLATORS: Screen-reader announcement and status bar text while open documents are being re-parsed after a settings change
			0 => (self.reparse_jobs_left > 0).then(|| t("Reloading documents…")),
			1 => {
				let entry = &self.pending[0];
				let name = title_or_filename(entry.title_override.clone().unwrap_or_default(), &entry.path);
				// TRANSLATORS: Status bar text while a document is being parsed; {} is the document title or file name
				Some(t("Loading {}…").replace("{}", &name))
			}
			// TRANSLATORS: Status bar text while several documents are being parsed; {} is how many
			count => Some(t("Loading {} documents…").replace("{}", &count.to_string())),
		}
	}

	/// Returns a live-region reminder every `REMINDER_INTERVAL` while work remains. The first busy
	/// poll starts the clock; it resets when the registry goes idle or new work starts from idle.
	pub fn due_reminder(&mut self, now: Instant) -> Option<String> {
		if self.pending.is_empty() && self.reparse_jobs_left == 0 {
			self.reminder_anchor = None;
			return None;
		}
		let anchor = *self.reminder_anchor.get_or_insert(now);
		if now.duration_since(anchor) < REMINDER_INTERVAL {
			return None;
		}
		self.reminder_anchor = Some(now);
		Some(if self.pending.is_empty() {
			// TRANSLATORS: Periodic screen-reader reminder that a document re-parse is still running
			t("Still reloading…")
		} else {
			// TRANSLATORS: Periodic screen-reader reminder that a document is still being loaded
			t("Still loading…")
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(path: &str, is_restore: bool) -> PendingParse {
		PendingParse {
			seq: 0,
			path: PathBuf::from(path),
			key: path.to_string(),
			track: true,
			is_restore,
			title_override: None,
			initial_caret: None,
			focus_when_done: false,
			password: String::new(),
			password_attempted: false,
			forced_extension: String::new(),
			render_tables_inline: true,
		}
	}

	#[test]
	fn register_assigns_increasing_seqs_and_finds_entries_by_key_and_seq() {
		let mut registry = ParseRegistry::default();
		let first = registry.register(entry("a.epub", false));
		let second = registry.register(entry("b.epub", false));

		assert!(first < second);
		assert_eq!(registry.by_key_mut("b.epub").unwrap().seq, second);
		assert_eq!(registry.by_seq_mut(first).unwrap().key, "a.epub");
		assert!(registry.by_key_mut("missing.epub").is_none());
	}

	#[test]
	fn user_open_cancels_restore_selection_but_restore_opens_keep_it() {
		let mut registry = ParseRegistry::default();
		registry.set_restore_active_key(Some("a.epub".to_string()));
		registry.register(entry("b.epub", true));
		assert_eq!(registry.restore_active_key(), Some("a.epub"));

		registry.register(entry("c.epub", false));
		assert_eq!(registry.restore_active_key(), None);
	}

	#[test]
	fn restore_group_finishes_when_its_last_entry_is_taken_and_counts_failures() {
		let mut registry = ParseRegistry::default();
		let first = registry.register(entry("a.epub", true));
		let second = registry.register(entry("b.epub", true));
		let user = registry.register(entry("c.epub", false));

		assert_eq!(registry.take(first, true).unwrap().1, None);
		// A failed user open does not count against startup restore.
		assert_eq!(registry.take(user, true).unwrap().1, None);
		assert_eq!(registry.take(second, false).unwrap().1, Some(1));
	}

	#[test]
	fn cancel_all_discards_pending_entries_and_restore_selection() {
		let mut registry = ParseRegistry::default();
		registry.set_restore_active_key(Some("a.epub".to_string()));
		let first = registry.register(entry("a.epub", true));
		let second = registry.register(entry("b.epub", false));

		registry.cancel_all();

		assert!(!registry.has_pending());
		assert_eq!(registry.restore_active_key(), None);
		assert!(registry.by_seq_mut(first).is_none());
		assert!(registry.take(second, false).is_none());
	}

	#[test]
	fn busy_status_text_reflects_what_is_in_flight() {
		let mut registry = ParseRegistry::default();
		assert_eq!(registry.busy_status_text(), None);

		let first = registry.register(entry("a.epub", false));
		assert_eq!(registry.busy_status_text(), Some("Loading a.epub…".to_string()));

		let second = registry.register(entry("b.epub", false));
		assert_eq!(registry.busy_status_text(), Some("Loading 2 documents…".to_string()));

		registry.take(first, false).unwrap();
		assert_eq!(registry.busy_status_text(), Some("Loading b.epub…".to_string()));

		// A synthetic document (e.g. View Source) is named by its tab title, not its temp file.
		registry.take(second, false).unwrap();
		registry.register(PendingParse {
			title_override: Some("Source: book.epub".to_string()),
			..entry("book.epub.source.txt", false)
		});
		assert_eq!(registry.busy_status_text(), Some("Loading Source: book.epub…".to_string()));
	}

	#[test]
	fn reparse_only_batch_shows_reloading_status() {
		let mut registry = ParseRegistry::default();
		registry.begin_reparse_batch(2);
		assert_eq!(registry.busy_status_text(), Some("Reloading documents…".to_string()));
	}

	#[test]
	fn reminders_fire_every_interval_while_busy_and_reset_when_idle() {
		let mut registry = ParseRegistry::default();
		let base = Instant::now();
		assert_eq!(registry.due_reminder(base), None);

		let seq = registry.register(entry("a.epub", false));
		// The first busy poll starts the clock without announcing.
		assert_eq!(registry.due_reminder(base), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(14)), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(15)), Some("Still loading…".to_string()));
		assert_eq!(registry.due_reminder(base + Duration::from_secs(16)), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(30)), Some("Still loading…".to_string()));

		// The next busy period starts a new interval after becoming idle.
		registry.take(seq, false).unwrap();
		assert_eq!(registry.due_reminder(base + Duration::from_secs(60)), None);
		registry.register(entry("b.epub", false));
		assert_eq!(registry.due_reminder(base + Duration::from_secs(61)), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(75)), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(76)), Some("Still loading…".to_string()));
	}

	#[test]
	fn busy_period_starting_between_polls_gets_a_fresh_reminder_clock() {
		let mut registry = ParseRegistry::default();
		let base = Instant::now();
		let seq = registry.register(entry("a.epub", false));
		assert_eq!(registry.due_reminder(base), None);

		// The registry goes idle and busy again without a poll observing the idle gap.
		registry.take(seq, false).unwrap();
		registry.register(entry("b.epub", false));

		assert_eq!(registry.due_reminder(base + Duration::from_secs(20)), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(35)), Some("Still loading…".to_string()));
	}

	#[test]
	fn reminder_wording_distinguishes_reparse_batches() {
		let mut registry = ParseRegistry::default();
		registry.begin_reparse_batch(1);
		let base = Instant::now();
		assert_eq!(registry.due_reminder(base), None);
		assert_eq!(registry.due_reminder(base + Duration::from_secs(15)), Some("Still reloading…".to_string()));
	}

	#[test]
	fn reparse_batch_counts_jobs_and_failures() {
		let mut registry = ParseRegistry::default();
		let generation = registry.begin_reparse_batch(2);

		assert_eq!(registry.reparse_job_done(generation, false), ReparseJobOutcome::BatchInFlight);
		assert_eq!(registry.reparse_job_done(generation, true), ReparseJobOutcome::BatchFinished { failed: 1 });
	}

	#[test]
	fn superseded_reparse_batch_is_discarded() {
		let mut registry = ParseRegistry::default();
		let stale = registry.begin_reparse_batch(2);
		let current = registry.begin_reparse_batch(1);

		assert_eq!(registry.reparse_job_done(stale, false), ReparseJobOutcome::Superseded);
		assert_eq!(registry.reparse_job_done(current, false), ReparseJobOutcome::BatchFinished { failed: 0 });
	}
}
