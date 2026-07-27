//! Tracks background document opens, startup restoration, and `render_tables_inline` re-parses.
//! `DocumentManager` owns this state; it contains no widgets or locks.

use std::path::PathBuf;

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

#[derive(Default)]
pub struct ParseRegistry {
	pending: Vec<PendingParse>,
	next_seq: u64,
	/// The startup restore's active tab. A user-initiated open (including re-selecting an
	/// already-open tab) clears it so restore cannot override the user's selection.
	restore_active_key: Option<String>,
	/// Identifies the current re-parse batch so results from older batches can be discarded.
	reparse_generation: u64,
	reparse_jobs_left: usize,
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

	/// Removes `seq` and reports whether it was the last startup-restore request. Returns `None`
	/// for an entry that was cancelled before its parse completed.
	pub fn take(&mut self, seq: u64) -> Option<(PendingParse, bool)> {
		let index = self.pending.iter().position(|entry| entry.seq == seq)?;
		let entry = self.pending.remove(index);
		let restore_done = entry.is_restore && !self.pending.iter().any(|e| e.is_restore);
		Some((entry, restore_done))
	}

	/// Cancels every pending open, including any startup restore; their in-flight parse results
	/// are discarded when they complete.
	pub fn cancel_all(&mut self) {
		self.pending.clear();
		self.restore_active_key = None;
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
		self.reparse_generation += 1;
		self.reparse_jobs_left = jobs;
		self.reparse_generation
	}

	/// Records a re-parse completion. Returns `None` for a superseded batch; otherwise reports
	/// whether the current batch has finished.
	pub const fn reparse_job_done(&mut self, generation: u64) -> Option<bool> {
		if generation != self.reparse_generation {
			return None;
		}
		self.reparse_jobs_left -= 1;
		Some(self.reparse_jobs_left == 0)
	}

	pub fn busy_status_text(&self) -> Option<String> {
		self.pending.last().map(|entry| {
			let filename = title_or_filename(String::new(), &entry.path);
			// TRANSLATORS: Status bar text while a document is being parsed; {} is the document title or file name
			t("Loading {}…").replace("{}", &filename)
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
	fn restore_group_finishes_when_its_last_entry_is_taken() {
		let mut registry = ParseRegistry::default();
		let first = registry.register(entry("a.epub", true));
		let second = registry.register(entry("b.epub", true));
		let user = registry.register(entry("c.epub", false));

		assert!(!registry.take(first).unwrap().1);
		// A non-restore open completing doesn't finish the group.
		assert!(!registry.take(user).unwrap().1);
		assert!(registry.take(second).unwrap().1);
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
		assert!(registry.take(second).is_none());
	}

	#[test]
	fn busy_status_text_shows_newest_pending_document() {
		let mut registry = ParseRegistry::default();
		assert_eq!(registry.busy_status_text(), None);

		registry.register(entry("a.epub", false));
		assert_eq!(registry.busy_status_text(), Some("Loading a.epub…".to_string()));

		let second = registry.register(entry("b.epub", false));
		assert_eq!(registry.busy_status_text(), Some("Loading b.epub…".to_string()));

		registry.take(second).unwrap();
		assert_eq!(registry.busy_status_text(), Some("Loading a.epub…".to_string()));
	}

	#[test]
	fn reparse_batch_counts_jobs_until_finished() {
		let mut registry = ParseRegistry::default();
		let generation = registry.begin_reparse_batch(2);

		assert_eq!(registry.reparse_job_done(generation), Some(false));
		assert_eq!(registry.reparse_job_done(generation), Some(true));
	}

	#[test]
	fn superseded_reparse_batch_is_discarded() {
		let mut registry = ParseRegistry::default();
		let stale = registry.begin_reparse_batch(2);
		let current = registry.begin_reparse_batch(1);

		assert_eq!(registry.reparse_job_done(stale), None);
		assert_eq!(registry.reparse_job_done(current), Some(true));
	}
}
