use std::{
	cell::{Cell, RefCell},
	env,
	path::Path,
	process,
	rc::Rc,
	sync::{
		Mutex,
		atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering},
	},
	time::{Instant, SystemTime, UNIX_EPOCH},
};

use paperback_core::{
	config::ConfigManager,
	parser::{PASSWORD_REQUIRED_ERROR_PREFIX, build_file_filter_string},
	session::DocumentSession,
	types::BookmarkFilterType,
};
use patois::t;
use wxdragon::{prelude::*, timer::Timer};

#[cfg(target_os = "windows")]
use super::tray;
use super::{
	dialogs,
	document_manager::{
		DocumentManager, ReparseInput, build_document_load_error_message, build_font_from_readability, display_title,
		normalized_path_key, prompt_for_password, show_error_dialog, title_or_filename,
	},
	find::{self, FindDialogState},
	help::{self, MAIN_WINDOW_PTR},
	menu, menu_ids,
	navigation::{self, MarkerNavTarget},
	parse_registry::{PendingParse, ReparseJobOutcome},
	status,
};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use crate::ipc::IpcCommand;
use crate::{
	config_ext::{UpdateChannel, get_update_channel, set_update_channel},
	translation_manager::TranslationManager,
};

const KEY_DELETE: i32 = 127;
const KEY_NUMPAD_DELETE: i32 = 330;

pub static SLEEP_TIMER_START_MS: AtomicI64 = AtomicI64::new(0);
pub static SLEEP_TIMER_DURATION_MINUTES: AtomicI32 = AtomicI32::new(0);

/// Prevents background-parse completions from touching widgets or config after final shutdown.
pub static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// How a document open should present itself. The default is a tracked, user-initiated open with
/// no title or caret override.
pub struct OpenRequest {
	/// Whether to remember the document in recent files, saved positions, and the restore list.
	/// Help and source views are untracked.
	pub track: bool,
	/// Set for opens replaying a previous session, which must not prompt about sidecar files the
	/// user already answered for.
	pub is_restore: bool,
	/// Tab title to use instead of the document's own, for synthetic documents like source views.
	pub title_override: Option<String>,
	/// Caret position to jump to once the tab exists.
	pub initial_caret: Option<i64>,
}

impl Default for OpenRequest {
	fn default() -> Self {
		Self { track: true, is_restore: false, title_override: None, initial_caret: None }
	}
}

#[derive(Default)]
struct RestoreState {
	restored: bool,
	closing: bool,
}

pub struct MainWindow {
	frame: Frame,
	doc_manager: Rc<Mutex<DocumentManager>>,
	config: Rc<Mutex<ConfigManager>>,
	#[cfg(target_os = "windows")]
	_tray_state: Rc<Mutex<Option<tray::TrayState>>>,
	live_region_label: StaticText,
	_find_dialog: Rc<Mutex<Option<FindDialogState>>>,
	/// Parse completions deferred to avoid re-locking a mutex held below a modal dialog's nested
	/// event loop. A frame-owned timer retries them after the modal closes; see `run_or_defer`.
	deferred_completions: Rc<RefCell<Vec<Box<dyn FnOnce()>>>>,
	#[cfg(target_os = "windows")]
	_hotkey_handle: Rc<RefCell<Option<HotkeyHandle>>>,
}

#[cfg(target_os = "windows")]
static HIDDEN_POPUP: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

impl MainWindow {
	pub fn new(config: Rc<Mutex<ConfigManager>>) -> Self {
		// TRANSLATORS: Main window title when no document is open
		let app_title = t("Paperback");
		let frame = Frame::builder().with_title(&app_title).with_size(Size::new(800, 600)).build();
		MAIN_WINDOW_PTR.store(frame.handle_ptr() as usize, Ordering::SeqCst);
		frame.create_status_bar(1, 0, -1, "statusbar");
		// TRANSLATORS: Default status bar text when no document is open
		frame.set_status_text(&t("Ready"), 0);
		let menu_bar = menu::create_menu_bar(&config.lock().unwrap());
		frame.set_menu_bar(menu_bar);
		menu::update_menu_item_states(&frame, false);
		menu::update_reopen_state(&frame, false);
		let panel = Panel::builder(&frame).build();
		let sizer = BoxSizer::builder(Orientation::Vertical).build();
		let live_region_label = StaticText::builder(&panel).with_label("").with_size(Size::new(0, 0)).build();
		live_region_label.show(false);
		let _ = live_region::set_live_region(&live_region_label);
		let notebook = Notebook::builder(&panel).with_style(NotebookStyle::Top).build();
		#[cfg(windows)]
		notebook.msw_disable_composited();
		sizer.add(&notebook, 1, SizerFlag::Expand | SizerFlag::All, 0);
		panel.set_sizer(sizer, true);
		let doc_manager =
			Rc::new(Mutex::new(DocumentManager::new(frame, notebook, Rc::clone(&config), live_region_label)));
		let find_dialog = Rc::new(Mutex::new(None));
		#[cfg(target_os = "windows")]
		let hotkey_handle = Rc::new(RefCell::new(start_hotkey_listener(&config.lock().unwrap().get_hotkey())));
		Self::bind_menu_events(
			&frame,
			&doc_manager,
			&config,
			&find_dialog,
			live_region_label,
			#[cfg(target_os = "windows")]
			&hotkey_handle,
		);
		let frame_copy = frame;
		let notebook = *doc_manager.lock().unwrap().notebook();
		let dm = Rc::clone(&doc_manager);
		notebook.on_page_changing(move |event| {
			let Ok(dm_ref) = dm.try_lock() else {
				return;
			};
			if !dm_ref.notebook().has_focus()
				&& let Some(new_index) = event.get_selection()
				&& let Ok(new_index) = usize::try_from(new_index)
				&& let Some(tab) = dm_ref.get_tab(new_index)
			{
				live_region::announce(live_region_label, &display_title(tab));
			}
		});
		let dm = Rc::clone(&doc_manager);
		notebook.on_page_changed(move |_event| {
			let Ok(dm_ref) = dm.try_lock() else {
				return;
			};
			update_title_from_manager(&frame_copy, &dm_ref);
			dm_ref.reset_sound_line();
		});
		let dm = Rc::clone(&doc_manager);
		let frame_copy = frame;
		notebook.on_key_down(move |event| {
			if let WindowEventData::Keyboard(key_event) = &event
				&& let Some(key) = key_event.get_key_code()
				&& (key == KEY_DELETE || key == KEY_NUMPAD_DELETE)
			{
				let mut dm = dm.lock().unwrap();
				close_active_document_announced(&mut dm, live_region_label);
				update_title_from_manager(&frame_copy, &dm);
				let has_docs = dm.tab_count() > 0;
				let has_reopen = dm.has_recently_closed();
				if has_docs {
					dm.restore_focus();
				} else {
					dm.notebook().set_focus();
				}
				drop(dm);
				menu::update_menu_item_states(&frame_copy, has_docs);
				menu::update_reopen_state(&frame_copy, has_reopen);
				event.skip(false);
				return;
			}
			event.skip(true);
		});
		#[cfg(target_os = "windows")]
		let tray_state = Rc::new(Mutex::new(None));
		#[cfg(target_os = "windows")]
		tray::bind_tray_events(frame, &doc_manager, &config, &tray_state);
		{
			let dm_for_close = Rc::clone(&doc_manager);
			let config_for_close = Rc::clone(&config);
			#[cfg(target_os = "windows")]
			let tray_for_close = Rc::clone(&tray_state);
			#[cfg(target_os = "windows")]
			let hotkey_for_close = Rc::clone(&hotkey_handle);
			frame.on_close(move |event| {
				let dm = dm_for_close.lock().unwrap();
				if let Some(tab) = dm.active_tab() {
					let path = tab.file_path.to_string_lossy();
					let cfg = config_for_close.lock().unwrap();
					cfg.set_app_string("active_document", &path);
					cfg.flush();
				}
				dm.save_all_positions();
				#[cfg(target_os = "macos")]
				if let WindowEventData::General(ref ev) = event {
					if ev.can_veto() {
						drop(dm);
						ev.veto();
						frame.show(false);
						return;
					}
				}
				#[cfg(target_os = "windows")]
				if let Some(state) = tray_for_close.lock().unwrap().as_ref() {
					state.icon.remove_icon();
				}
				#[cfg(target_os = "windows")]
				if let Some(handle) = hotkey_for_close.borrow_mut().take() {
					use windows::Win32::{
						Foundation::{LPARAM, WPARAM},
						UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT},
					};
					if handle.thread_id != 0 {
						unsafe {
							let _ = PostThreadMessageW(handle.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
						}
					}
				}
				// Reaching this point means macOS did not veto the close.
				SHUTTING_DOWN.store(true, Ordering::SeqCst);
				event.skip(true);
			});
		}
		frame.on_destroy(move |_event| {
			SHUTTING_DOWN.store(true, Ordering::SeqCst);
		});
		#[cfg(target_os = "windows")]
		{
			let tray_for_destroy = Rc::clone(&tray_state);
			frame.on_destroy(move |_event| {
				if let Some(state) = tray_for_destroy.lock().unwrap().take() {
					state.icon.destroy();
				}
			});
		}
		Self::schedule_restore_documents(frame, Rc::clone(&doc_manager), Rc::clone(&config));
		let deferred_completions: Rc<RefCell<Vec<Box<dyn FnOnce()>>>> = Rc::new(RefCell::new(Vec::new()));
		// This timer paces long-running reminders and retries deferred completions. Inside a modal
		// event loop, the lock probe keeps completions queued until the outer handler returns.
		let drain_timer = Rc::new(Timer::new(&frame));
		let deferred = Rc::clone(&deferred_completions);
		drain_timer.on_tick(move |_| {
			if SHUTTING_DOWN.load(Ordering::SeqCst) {
				return;
			}
			let Some(window) = super::app::main_window_from_ptr() else {
				return;
			};
			if let Ok(mut dm) = window.doc_manager.try_lock()
				&& let Some(reminder) = dm.parses_mut().due_reminder(Instant::now())
			{
				drop(dm);
				live_region::announce(window.live_region_label, &reminder);
			}
			if deferred.borrow().is_empty() {
				return;
			}
			if window.doc_manager.try_lock().is_err() || window.config.try_lock().is_err() {
				return;
			}
			let ready: Vec<_> = deferred.borrow_mut().drain(..).collect();
			for completion in ready {
				completion();
			}
		});
		drain_timer.start(150, false);
		// Keep the timer in this frame-bound closure. A wx timer that outlives its frame continues
		// dispatching into the freed event handler.
		frame.on_destroy(move |_event| {
			drain_timer.stop();
		});
		Self {
			frame,
			doc_manager,
			config,
			#[cfg(target_os = "windows")]
			_tray_state: tray_state,
			live_region_label,
			_find_dialog: find_dialog,
			deferred_completions,
			#[cfg(target_os = "windows")]
			_hotkey_handle: hotkey_handle,
		}
	}

	pub fn show(&self) {
		if self.config.lock().unwrap().get_app_bool("start_maximized", false) {
			self.frame.maximize(true);
		}
		self.frame.show(true);
		self.frame.centre();
	}

	#[cfg(target_os = "macos")]
	pub fn show_from_dock(&self) {
		self.frame.show(true);
		self.frame.raise();
		self.doc_manager.lock().unwrap().restore_focus();
	}

	pub fn check_for_updates(silent: bool, channel: UpdateChannel) {
		help::run_update_check(silent, channel);
	}

	pub fn open_file(&self, path: &Path) -> bool {
		self.request_open(path, OpenRequest::default())
	}

	/// Submits a document open. Everything that needs the user -- choosing a format, importing a
	/// sidecar file -- happens here, on the UI thread, before the parse is handed to a worker
	/// thread; `finish_parse` applies the result. Returns `false` only for a file that cannot be
	/// opened at all, since a parse failure is not known yet.
	pub(super) fn request_open(&self, path: &Path, request: OpenRequest) -> bool {
		// Resolving the format writes it to config, so this must precede reading the parse inputs
		// below. Startup restore resolves its whole list up front, which makes this a no-op there.
		if !dialogs::ensure_parser_ready_for_path(&self.frame, path, &self.config) {
			return false;
		}
		// The "Open As" prompt's nested event loop may have closed the window.
		if SHUTTING_DOWN.load(Ordering::SeqCst) {
			return true;
		}
		let notebook = *self.doc_manager.lock().unwrap().notebook();
		if !path.exists() {
			// TRANSLATORS: Error message shown when the requested document file does not exist; {} is the file path
			let message = t("File not found: {}").replace("{}", &path.to_string_lossy());
			// TRANSLATORS: Generic error dialog title
			show_error_dialog(&notebook, &message, &t("Error"));
			return false;
		}
		let key = normalized_path_key(path);
		let is_restore = request.is_restore;
		// The name announced below; synthetic documents (e.g. View Source) go by their tab title,
		// not their temp file. Captured before the request's title moves into the registry.
		let display_name = title_or_filename(request.title_override.clone().unwrap_or_default(), path);
		let seq = {
			let mut dm = self.doc_manager.lock().unwrap();
			if let Some(index) = dm.find_tab_by_key(&key) {
				// Bring the existing tab forward rather than opening the same document twice.
				dm.notebook().set_selection(index);
				if let Some(caret) = request.initial_caret
					&& let Some(tab) = dm.get_tab(index)
				{
					tab.text_ctrl.set_insertion_point(caret);
					tab.text_ctrl.show_position(caret);
				}
				if !is_restore {
					// Do not let startup restore override the user's selection.
					dm.parses_mut().set_restore_active_key(None);
				}
				dm.restore_focus();
				update_title_from_manager(&self.frame, &dm);
				return true;
			}
			// Reuse the pending open and apply the latest focus and caret request when it finishes.
			if let Some(pending) = dm.parses_mut().by_key_mut(&key) {
				if !is_restore {
					pending.focus_when_done = true;
				}
				if request.initial_caret.is_some() {
					pending.initial_caret = request.initial_caret;
				}
				return true;
			}
			// Register before opening a modal. Its nested event loop may submit the same path again
			// through IPC or startup restore. The sidecar import below may still change parse inputs.
			dm.parses_mut().register(PendingParse {
				seq: 0,
				path: path.to_path_buf(),
				key,
				track: request.track,
				is_restore,
				title_override: request.title_override,
				initial_caret: request.initial_caret,
				focus_when_done: false,
				password: String::new(),
				password_attempted: false,
				forced_extension: String::new(),
				render_tables_inline: true,
			})
		};
		let import_path = path.with_extension("paperback");
		if !is_restore && import_path.exists() {
			// TRANSLATORS: Prompt asking whether to import a document's previously saved settings and bookmarks found alongside it
			let message = t("A .paperback file was found for this document. Would you like to import it?");
			// TRANSLATORS: Title of the dialog prompting to import a document's saved settings and bookmarks
			let title = t("Import document data");
			let dialog = MessageDialog::builder(&notebook, &message, &title)
				.with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion | MessageDialogStyle::Centre)
				.build();
			if dialog.show_modal() == ID_YES {
				let config = self.config.lock().unwrap();
				config.import_settings_from_file(&path.to_string_lossy(), import_path.to_str().unwrap());
			}
			// The import prompt's nested event loop may have closed the window.
			if SHUTTING_DOWN.load(Ordering::SeqCst) {
				return true;
			}
		}
		let (password, forced_extension, render_tables_inline) = {
			let config = self.config.lock().unwrap();
			let path_str = path.to_string_lossy();
			config.refresh_document_hash(&path_str);
			(
				config.get_document_password(&path_str),
				config.get_document_format(&path_str),
				config.get_app_bool("render_tables_inline", true),
			)
		};
		tracing::info!(path = %path.display(), "opening document");
		let status = {
			let mut dm = self.doc_manager.lock().unwrap();
			// The import prompt's event loop may also have run a Close All that cancelled this open.
			let Some(entry) = dm.parses_mut().by_seq_mut(seq) else {
				return true;
			};
			entry.password.clone_from(&password);
			entry.forced_extension.clone_from(&forced_extension);
			entry.render_tables_inline = render_tables_inline;
			dm.parses().busy_status_text()
		};
		// Announce direct opens by name. Startup restore stays quiet, while the status bar always
		// shows the overall busy state.
		if !is_restore {
			// TRANSLATORS: Screen-reader announcement when a document starts loading; {} is the document title or file name
			live_region::announce(self.live_region_label, &t("Loading {}…").replace("{}", &display_name));
		}
		if let Some(status) = status {
			self.frame.set_status_text(&status, 0);
		}
		spawn_parse(seq, path.to_string_lossy().to_string(), password, forced_extension, render_tables_inline);
		true
	}

	/// Runs a parse completion unless a modal event loop re-entered the UI while an outer handler
	/// holds a required mutex. Since standard mutexes are not reentrant, the drain timer must retry
	/// the completion after that handler returns.
	fn run_or_defer(&self, completion: Box<dyn FnOnce()>) {
		if self.doc_manager.try_lock().is_ok() && self.config.try_lock().is_ok() {
			completion();
		} else {
			self.deferred_completions.borrow_mut().push(completion);
		}
	}

	/// Handles a background parse on the UI thread. It must release the document-manager lock
	/// before opening a modal or calling `update_recent_documents_menu`, which locks the manager.
	pub(super) fn finish_parse(&self, seq: u64, result: Result<DocumentSession, String>) {
		match result {
			Ok(session) => {
				let (is_restore, restore_finished, loaded_title) = {
					let mut dm = self.doc_manager.lock().unwrap();
					// A missing entry means the open was cancelled; discard the result.
					let Some(entry) = dm.parses_mut().by_seq_mut(seq) else {
						return;
					};
					// Re-parse if the table setting changed while this session was being built.
					let current = self.config.lock().unwrap().get_app_bool("render_tables_inline", true);
					if entry.render_tables_inline != current {
						entry.render_tables_inline = current;
						spawn_parse(
							seq,
							entry.path.to_string_lossy().to_string(),
							entry.password.clone(),
							entry.forced_extension.clone(),
							current,
						);
						return;
					}
					let Some((entry, restore_finished)) = dm.parses_mut().take(seq, false) else {
						return;
					};
					let select = !entry.is_restore
						|| entry.focus_when_done
						|| dm.parses().restore_active_key() == Some(entry.key.as_str());
					let index = dm.add_session_tab(&self.doc_manager, &entry, session, select);
					if let Some(caret) = entry.initial_caret {
						let tab = dm.get_tab(index).unwrap();
						tab.text_ctrl.set_insertion_point(caret);
						tab.text_ctrl.show_position(caret);
					}
					if restore_finished.is_some() {
						dm.finish_restore_group();
						dm.restore_focus();
					} else if select {
						dm.restore_focus();
					}
					update_title_from_manager(&self.frame, &dm);
					// Announce a restored tab only if the user requested it again while it was pending.
					let loaded_title =
						(!entry.is_restore || entry.focus_when_done).then(|| display_title(dm.get_tab(index).unwrap()));
					(entry.is_restore, restore_finished, loaded_title)
				};
				let loaded_message = loaded_title.map(|title| {
					// TRANSLATORS: Screen-reader announcement when a document finishes loading; {} is the document title
					t("Document loaded: {}.").replace("{}", &title)
				});
				let group_message = restore_finished.map(|failed| {
					if failed == 0 {
						// TRANSLATORS: Screen-reader announcement when the startup restore group finishes and every document loaded
						t("All documents loaded.")
					} else {
						// TRANSLATORS: Screen-reader announcement when the startup restore group finishes but at least one document failed to load
						t("Document loading complete.")
					}
				});
				// A document completion that also finishes its restore group makes one combined
				// announce call: on macOS, a High-priority announcement interrupts and flushes the
				// one before it, so back-to-back calls would swallow the document title.
				let announcement = match (loaded_message, group_message) {
					(Some(loaded), Some(group)) => Some(format!("{loaded} {group}")),
					(loaded, group) => loaded.or(group),
				};
				if let Some(message) = announcement {
					live_region::announce(self.live_region_label, &message);
				}
				// Rebuild recent documents once per user open or restore group, not per restored tab.
				if !is_restore || restore_finished.is_some() {
					self.update_recent_documents_menu();
				} else {
					menu::update_menu_item_states(&self.frame, true);
				}
			}
			Err(err) => {
				let (notebook, path, prompt_password) = {
					let mut dm = self.doc_manager.lock().unwrap();
					let notebook = *dm.notebook();
					// A missing entry means the open was cancelled; discard the result.
					let Some(entry) = dm.parses_mut().by_seq_mut(seq) else {
						return;
					};
					let prompt_password = err.starts_with(PASSWORD_REQUIRED_ERROR_PREFIX) && !entry.password_attempted;
					(notebook, entry.path.clone(), prompt_password)
				};
				if prompt_password {
					self.config.lock().unwrap().set_document_password(&path.to_string_lossy(), "");
					let password = prompt_for_password(&notebook, &path);
					// The password prompt's nested event loop may have closed the window.
					if SHUTTING_DOWN.load(Ordering::SeqCst) {
						return;
					}
					if let Some(password) = password {
						let (path_str, forced_extension, render_tables_inline) = {
							let mut dm = self.doc_manager.lock().unwrap();
							// The prompt's event loop may also have run a Close All that cancelled this open.
							let Some(entry) = dm.parses_mut().by_seq_mut(seq) else {
								return;
							};
							entry.password.clone_from(&password);
							entry.password_attempted = true;
							(
								entry.path.to_string_lossy().to_string(),
								entry.forced_extension.clone(),
								entry.render_tables_inline,
							)
						};
						spawn_parse(seq, path_str, password, forced_extension, render_tables_inline);
						return;
					}
				}
				let Some((_, restore_finished)) = self.doc_manager.lock().unwrap().parses_mut().take(seq, true) else {
					return;
				};
				let message = if prompt_password {
					// TRANSLATORS: Error shown when the user dismisses the password prompt for an encrypted document without entering one
					t("Password is required.")
				} else {
					tracing::error!(path = %path.display(), error = %err, "failed to open document");
					build_document_load_error_message(&path, &err)
				};
				show_error_dialog(&notebook, &message, &t("Error"));
				if SHUTTING_DOWN.load(Ordering::SeqCst) {
					return;
				}
				let mut dm = self.doc_manager.lock().unwrap();
				if restore_finished.is_some() {
					dm.finish_restore_group();
					dm.restore_focus();
				}
				update_title_from_manager(&self.frame, &dm);
				drop(dm);
				if let Some(failed) = restore_finished {
					let message = if failed == 0 {
						// TRANSLATORS: Screen-reader announcement when the startup restore group finishes and every document loaded
						t("All documents loaded.")
					} else {
						// TRANSLATORS: Screen-reader announcement when the startup restore group finishes but at least one document failed to load
						t("Document loading complete.")
					};
					live_region::announce(self.live_region_label, &message);
				}
			}
		}
	}

	/// Re-parses every open document after `render_tables_inline` changes. This uses the shared
	/// parse-time renderer; a failed re-parse leaves its tab unchanged.
	pub(super) fn request_render_tables_reparse(&self, render_tables_inline: bool) {
		let (inputs, generation) = {
			let mut dm = self.doc_manager.lock().unwrap();
			let inputs = dm.collect_reparse_inputs();
			let generation = dm.parses_mut().begin_reparse_batch(inputs.len());
			if inputs.is_empty() {
				// An empty batch supersedes an in-flight one without ever reporting BatchFinished,
				// so any stale "Reloading documents…" status must be cleared here.
				update_title_from_manager(&self.frame, &dm);
				return;
			}
			(inputs, generation)
		};
		// TRANSLATORS: Screen-reader announcement and status bar text while open documents are being re-parsed after a settings change
		let message = t("Reloading documents…");
		live_region::announce(self.live_region_label, &message);
		self.frame.set_status_text(&message, 0);
		for input in inputs {
			std::thread::spawn(move || {
				let result =
					DocumentSession::new(&input.path, &input.password, &input.forced_extension, render_tables_inline);
				post_completion(move |window| window.finish_reparse(generation, input, result));
			});
		}
	}

	pub(super) fn finish_reparse(&self, generation: u64, input: ReparseInput, result: Result<DocumentSession, String>) {
		let mut dm = self.doc_manager.lock().unwrap();
		// Discard results from an older setting change.
		let outcome = dm.parses_mut().reparse_job_done(generation, result.is_err());
		if outcome == ReparseJobOutcome::Superseded {
			return;
		}
		match result {
			Ok(session) => dm.replace_tab_session(input.seq, session),
			Err(err) => {
				tracing::error!(path = %input.path, error = %err, "failed to re-parse document for render_tables_inline toggle");
			}
		}
		if let ReparseJobOutcome::BatchFinished { failed } = outcome {
			update_title_from_manager(&self.frame, &dm);
			drop(dm);
			// A failed re-parse keeps the old session, so do not announce a successful reload.
			let message = if failed == 0 {
				// TRANSLATORS: Screen-reader announcement when every open document has finished re-parsing
				t("Documents reloaded.")
			} else {
				// TRANSLATORS: Screen-reader announcement when re-parsing finishes but at least one document failed to reload
				t("Document reload complete.")
			};
			live_region::announce(self.live_region_label, &message);
		}
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	pub fn handle_ipc_command(&self, command: IpcCommand) {
		tracing::info!(command = ?command, "received IPC command");
		let mut web_view_dialog = None;
		dialogs::ACTIVE_WEB_VIEW.with(|v| {
			web_view_dialog = v.get();
		});

		if let Some(parent_dialog) = web_view_dialog {
			let dialog = MessageDialog::builder(
				&parent_dialog,
				// TRANSLATORS: Message shown when the user tries to perform an action while a help/documentation Web View window is open
				&t("Paperback cannot perform any actions while Web View is open."),
				// TRANSLATORS: Title of a warning dialog
				&t("Warning"),
			)
			.with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning | MessageDialogStyle::Centre)
			.build();
			dialog.show_modal();
			return;
		}

		match command {
			IpcCommand::Activate => {
				self.activate_from_ipc();
			}
			IpcCommand::ToggleVisibility => {
				self.toggle_visibility();
			}
			IpcCommand::OpenFile(path) => {
				self.activate_from_ipc();
				self.open_file(&path);
				self.frame.raise();
				self.doc_manager.lock().unwrap().restore_focus();
			}
		}
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn toggle_visibility(&self) {
		let is_shown = self.frame.is_shown();
		if is_shown && self.is_window_active() {
			let mut has_popup = false;
			#[cfg(target_os = "windows")]
			{
				use windows::Win32::{
					Foundation::HWND,
					UI::WindowsAndMessaging::{GetLastActivePopup, SW_HIDE, ShowWindow},
				};
				let handle = self.frame.get_handle();
				if !handle.is_null() {
					let frame_hwnd = HWND(handle);
					let active_popup = unsafe { GetLastActivePopup(frame_hwnd) };
					if active_popup != frame_hwnd {
						has_popup = true;
						HIDDEN_POPUP.store(active_popup.0 as isize, Ordering::SeqCst);
						let _ = unsafe { ShowWindow(active_popup, SW_HIDE) };
					}
				}
			}

			if has_popup {
				self.frame.show(false);
			} else {
				self.frame.iconize(true);
			}
		} else {
			self.activate_from_ipc();
		}
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn activate_from_ipc(&self) {
		self.frame.show(true);
		self.frame.iconize(false);
		self.frame.request_user_attention(UserAttentionFlag::Info);
		self.frame.raise();

		#[allow(unused_mut)]
		let mut has_popup = false;
		#[cfg(target_os = "windows")]
		{
			use windows::Win32::{
				Foundation::HWND,
				UI::WindowsAndMessaging::{GetLastActivePopup, SW_SHOW, SetForegroundWindow, ShowWindow},
			};
			let handle = self.frame.get_handle();
			if !handle.is_null() {
				let frame_hwnd = HWND(handle);

				let hidden = HIDDEN_POPUP.swap(0, Ordering::SeqCst);
				if hidden != 0 {
					let active_popup = HWND(hidden as _);
					let _ = unsafe { ShowWindow(active_popup, SW_SHOW) };
					let _ = unsafe { SetForegroundWindow(active_popup) };
					has_popup = true;
				} else {
					let active_popup = unsafe { GetLastActivePopup(frame_hwnd) };
					has_popup = active_popup != frame_hwnd;

					let _ = unsafe { SetForegroundWindow(active_popup) };
				}
			}
		}

		if !has_popup {
			self.doc_manager.lock().unwrap().restore_focus();
		}

		#[cfg(not(target_os = "linux"))]
		if let Some(state) = self._tray_state.lock().unwrap().as_mut() {
			if let Some(bundle) =
				ArtProvider::get_bitmap_bundle(ArtId::Information, ArtClient::MessageBox, Some(Size::new(32, 32)))
			{
				state.icon.set_icon_bundle(&bundle, "Paperback");
			} else if let Some(bitmap) =
				ArtProvider::get_bitmap(ArtId::Information, ArtClient::MessageBox, Some(Size::new(32, 32)))
			{
				state.icon.set_icon(&bitmap, "Paperback");
			}
		}
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn is_window_active(&self) -> bool {
		#[cfg(target_os = "windows")]
		{
			use windows::Win32::{
				Foundation::HWND,
				UI::WindowsAndMessaging::{GetForegroundWindow, GetLastActivePopup},
			};
			let handle = self.frame.get_handle();
			if handle.is_null() {
				return self.frame.has_focus();
			}
			let frame_hwnd = HWND(handle);
			let foreground = unsafe { GetForegroundWindow() };
			let active_popup = unsafe { GetLastActivePopup(frame_hwnd) };
			foreground == frame_hwnd || foreground == active_popup
		}
		#[cfg(not(target_os = "windows"))]
		{
			self.frame.has_focus()
		}
	}

	/// Get the frame
	pub const fn frame(&self) -> &Frame {
		&self.frame
	}

	fn update_recent_documents_menu(&self) {
		let menu_bar = menu::create_menu_bar(&self.config.lock().unwrap());
		self.frame.set_menu_bar(menu_bar);
		let dm_ref = self.doc_manager.lock().unwrap();
		let has_docs = dm_ref.tab_count() > 0;
		let has_reopen = dm_ref.has_recently_closed();
		drop(dm_ref);
		menu::update_menu_item_states(&self.frame, has_docs);
		menu::update_reopen_state(&self.frame, has_reopen);
	}

	fn schedule_restore_documents(
		frame: Frame,
		doc_manager: Rc<Mutex<DocumentManager>>,
		config: Rc<Mutex<ConfigManager>>,
	) {
		let restore = config.lock().unwrap().get_app_bool("restore_previous_documents", true);
		if !restore {
			return;
		}
		let state = Rc::new(Mutex::new(RestoreState::default()));
		let state_for_close = Rc::clone(&state);
		frame.on_close(move |_event| {
			state_for_close.lock().unwrap().closing = true;
		});
		let state_for_destroy = Rc::clone(&state);
		frame.on_destroy(move |_event| {
			state_for_destroy.lock().unwrap().closing = true;
		});
		let state_for_idle = Rc::clone(&state);
		frame.on_idle(move |_event| {
			let mut state = state_for_idle.lock().unwrap();
			if state.restored || state.closing {
				return;
			}
			state.restored = true;
			drop(state);
			let active_path = config.lock().unwrap().get_app_string("active_document", "");
			let paths = config.lock().unwrap().get_opened_documents_existing();
			tracing::info!(count = paths.len(), "restoring previously open documents");
			{
				// A document opened before restore, through the CLI or IPC, keeps its selection.
				let mut dm = doc_manager.lock().unwrap();
				if dm.tab_count() == 0 && !dm.parses().has_pending() && !active_path.is_empty() {
					dm.parses_mut().set_restore_active_key(Some(normalized_path_key(Path::new(&active_path))));
				}
			}
			let window = super::app::main_window_from_ptr().expect("idle events only fire after the app is built");
			// Resolve every "Open As" prompt before submitting any parse. Otherwise an earlier parse
			// could finish inside a later prompt's event loop and appear to end the restore group.
			// `request_open` re-checks each path, but a resolved format is saved to config, so that
			// check finds it and opens no dialog.
			let mut ready = Vec::new();
			for path in paths {
				// A prompt's nested event loop may have closed the window.
				if SHUTTING_DOWN.load(Ordering::SeqCst) {
					return;
				}
				if dialogs::ensure_parser_ready_for_path(&frame, Path::new(&path), &config) {
					ready.push(path);
				}
			}
			if SHUTTING_DOWN.load(Ordering::SeqCst) {
				return;
			}
			for path in &ready {
				window.request_open(Path::new(path), OpenRequest { is_restore: true, ..OpenRequest::default() });
			}
			menu::update_reopen_state(&frame, false);
		});
	}

	fn handle_open(frame: &Frame) {
		let wildcard = build_file_filter_string();
		// TRANSLATORS: Title of the file picker dialog shown when opening a document
		let dialog_title = t("Open Document");
		let dialog = FileDialog::builder(frame)
			.with_message(&dialog_title)
			.with_wildcard(&wildcard)
			.with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
			.build();
		if dialog.show_modal() == ID_OK
			&& let Some(path) = dialog.get_path()
		{
			let window = super::app::main_window_from_ptr().expect("menu events only fire after the app is built");
			window.request_open(Path::new(&path), OpenRequest::default());
		}
	}

	#[allow(clippy::too_many_lines)]
	fn bind_menu_events(
		frame: &Frame,
		doc_manager: &Rc<Mutex<DocumentManager>>,
		config: &Rc<Mutex<ConfigManager>>,
		find_dialog: &Rc<Mutex<Option<FindDialogState>>>,
		live_region_label: StaticText,
		#[cfg(target_os = "windows")] hotkey_handle: &Rc<RefCell<Option<HotkeyHandle>>>,
	) {
		let frame_copy = *frame;
		let dm = Rc::clone(doc_manager);
		let config = Rc::clone(config);
		let find_dialog = Rc::clone(find_dialog);
		#[cfg(target_os = "windows")]
		let hotkey_handle_for_options = Rc::clone(hotkey_handle);
		let sleep_timer = Rc::new(Timer::new(frame));
		let sleep_timer_running = Rc::new(Cell::new(false));
		let sleep_timer_start_time = Rc::new(Cell::new(0i64));
		let sleep_timer_duration_minutes = Rc::new(Cell::new(0i32));
		let sleep_timer_for_tick = Rc::clone(&sleep_timer);
		let sleep_timer_running_for_tick = Rc::clone(&sleep_timer_running);
		let frame_for_timer = *frame;
		let dm_for_timer = Rc::clone(doc_manager);
		let config_for_timer = Rc::clone(&config);
		sleep_timer.on_tick(move |_| {
			// Timer tick closures are bound to the frame without a timer id, so every closure
			// runs for every frame-owned timer's tick, not just its own. Act only when a
			// sleep timer was set AND it actually fired: a pending one-shot wx timer reports
			// is_running() until it fires, so a still-running sleep timer means this dispatch
			// came from some other timer's tick.
			if !sleep_timer_running_for_tick.get() || sleep_timer_for_tick.is_running() {
				return;
			}
			tracing::info!("sleep timer fired, closing application");
			sleep_timer_running_for_tick.set(false);
			sleep_timer_for_tick.stop();
			SLEEP_TIMER_START_MS.store(0, Ordering::SeqCst);
			SLEEP_TIMER_DURATION_MINUTES.store(0, Ordering::SeqCst);
			{
				let dm = dm_for_timer.lock().unwrap();
				let cfg = config_for_timer.lock().unwrap();
				for i in 0..dm.tab_count() {
					if let Some(tab) = dm.get_tab(i) {
						let current_pos = tab.text_ctrl.get_insertion_point();
						let path_str = tab.file_path.to_string_lossy();
						cfg.set_document_position(&path_str, current_pos);
					}
				}
				cfg.flush();
			}
			frame_for_timer.close(true);
		});
		let status_update_timer = Rc::new(Timer::new(frame));
		let sleep_timer_running_for_status = Rc::clone(&sleep_timer_running);
		let sleep_timer_start_for_status = Rc::clone(&sleep_timer_start_time);
		let sleep_timer_duration_for_status = Rc::clone(&sleep_timer_duration_minutes);
		let dm_for_status = Rc::clone(doc_manager);
		let frame_for_status = *frame;
		status_update_timer.on_tick(move |_| {
			if !sleep_timer_running_for_status.get() {
				return;
			}
			let Ok(dm) = dm_for_status.try_lock() else {
				return;
			};
			status::update_status_bar_with_sleep_timer(
				&frame_for_status,
				&dm,
				sleep_timer_start_for_status.get(),
				sleep_timer_duration_for_status.get(),
			);
		});
		status_update_timer.start(1000, false);
		let sleep_timer_for_menu = Rc::clone(&sleep_timer);
		let sleep_timer_running_for_menu = Rc::clone(&sleep_timer_running);
		let sleep_timer_start_for_menu = Rc::clone(&sleep_timer_start_time);
		let sleep_timer_duration_for_menu = Rc::clone(&sleep_timer_duration_minutes);
		frame.on_menu(move |event| {
			let id = event.get_id();
			match id {
				menu_ids::OPEN => {
					Self::handle_open(&frame_copy);
				}
				menu_ids::CLOSE => {
					let mut dm = dm.lock().unwrap();
					close_active_document_announced(&mut dm, live_region_label);
					update_title_from_manager(&frame_copy, &dm);
					let has_docs = dm.tab_count() > 0;
					if has_docs {
						dm.restore_focus();
					} else {
						dm.notebook().set_focus();
					}
					drop(dm);
					menu::update_menu_item_states(&frame_copy, has_docs);
					menu::update_reopen_state(&frame_copy, true);
				}
				menu_ids::CLOSE_ALL => {
					let mut dm = dm.lock().unwrap();
					dm.close_all_documents();
					update_title_from_manager(&frame_copy, &dm);
					dm.notebook().set_focus();
					drop(dm);
					menu::update_menu_item_states(&frame_copy, false);
					menu::update_reopen_state(&frame_copy, true);
				}
				menu_ids::REOPEN_LAST_CLOSED => {
					let path = dm.lock().unwrap().pop_recently_closed();
					if let Some(path) = path {
						let window =
							super::app::main_window_from_ptr().expect("menu events only fire after the app is built");
						// A reopen entry never needs the "Open As" prompt: the stack holds
						// only tracked documents opened this run, and the only way to lose
						// a remembered format — removing the document from history — also
						// removes it from the stack. A false return therefore means the
						// file itself is unopenable, and the entry is dropped rather than
						// retried.
						window.request_open(&path, OpenRequest::default());
						let has_reopen = dm.lock().unwrap().has_recently_closed();
						menu::update_reopen_state(&frame_copy, has_reopen);
					}
				}
				menu_ids::EXIT => {
					dm.lock().unwrap().save_all_positions();
					process::exit(0);
				}
				menu_ids::FIND => {
					find::show_find_dialog(&frame_copy, &dm, &config, &find_dialog, live_region_label);
				}
				menu_ids::FIND_NEXT => {
					find::handle_find_action(&frame_copy, &dm, &config, &find_dialog, live_region_label, true);
				}
				menu_ids::FIND_PREVIOUS => {
					find::handle_find_action(&frame_copy, &dm, &config, &find_dialog, live_region_label, false);
				}
				menu_ids::GO_TO_LINE => {
					let (current_line, max_lines) = {
						let mut dm_guard = dm.lock().unwrap();
						let (current_line, max_lines) = {
							let Some(tab) = dm_guard.active_tab_mut() else {
								return;
							};
							let current_pos = tab.text_ctrl.get_insertion_point();
							let status = tab.session.get_status_info(current_pos);
							let total_lines = tab.session.line_count().max(1);
							let max_lines = i32::try_from(total_lines.min(i64::from(i32::MAX))).unwrap_or(i32::MAX);
							let current_line =
								i32::try_from(status.line_number.clamp(1, total_lines).min(i64::from(i32::MAX)))
									.unwrap_or(i32::MAX);
							(current_line, max_lines)
						};
						drop(dm_guard);
						(current_line, max_lines)
					};
					if let Some(line) = dialogs::show_go_to_line_dialog(&frame_copy, current_line, max_lines) {
						let (history, history_index, path_str) = {
							let mut dm_guard = dm.lock().unwrap();
							let (history, history_index, path_str) = {
								let Some(tab) = dm_guard.active_tab_mut() else {
									return;
								};
								let target_pos = tab.session.position_from_line(i64::from(line));
								tab.text_ctrl.set_focus();
								tab.text_ctrl.set_insertion_point(target_pos);
								tab.text_ctrl.show_position(target_pos);
								tab.session.check_and_record_history(target_pos);
								let (history, history_index) = tab.session.get_history();
								let history = history.to_vec();
								let path_str = tab.file_path.to_string_lossy().to_string();
								(history, history_index, path_str)
							};
							drop(dm_guard);
							(history, history_index, path_str)
						};
						let cfg = config.lock().unwrap();
						cfg.set_navigation_history(&path_str, &history, history_index);
					}
				}
				menu_ids::GO_TO_PAGE => {
					let (current_page, max_page) = {
						let mut dm_guard = dm.lock().unwrap();
						let (current_page, max_page) = {
							let Some(tab) = dm_guard.active_tab_mut() else {
								return;
							};
							let page_count = tab.session.page_count();
							if page_count == 0 {
								// TRANSLATORS: Announced when "Go to Page" is used on a document that has no page numbers
								live_region::announce(live_region_label, &t("No pages."));
								return;
							}
							let current_pos = tab.text_ctrl.get_insertion_point();
							let current_page = tab.session.current_page(current_pos);
							let max_page = i32::try_from(page_count.max(1)).unwrap_or(i32::MAX);
							(current_page, max_page)
						};
						drop(dm_guard);
						(current_page, max_page)
					};
					if let Some(page) = dialogs::show_go_to_page_dialog(&frame_copy, current_page, max_page) {
						let (history, history_index, path_str) = {
							let mut dm_guard = dm.lock().unwrap();
							let (history, history_index, path_str) = {
								let Some(tab) = dm_guard.active_tab_mut() else {
									return;
								};
								let target_pos = tab.session.page_offset(page);
								tab.text_ctrl.set_focus();
								tab.text_ctrl.set_insertion_point(target_pos);
								tab.text_ctrl.show_position(target_pos);
								tab.session.check_and_record_history(target_pos);
								let (history, history_index) = tab.session.get_history();
								let history = history.to_vec();
								let path_str = tab.file_path.to_string_lossy().to_string();
								(history, history_index, path_str)
							};
							drop(dm_guard);
							(history, history_index, path_str)
						};
						let cfg = config.lock().unwrap();
						cfg.set_navigation_history(&path_str, &history, history_index);
					}
				}
				menu_ids::GO_TO_PERCENT => {
					let current_percent = {
						let mut dm_guard = dm.lock().unwrap();
						let current_percent = {
							let Some(tab) = dm_guard.active_tab_mut() else {
								return;
							};
							let current_pos = tab.text_ctrl.get_insertion_point();
							let status = tab.session.get_status_info(current_pos);
							status.percentage.clamp(0, 100)
						};
						drop(dm_guard);
						current_percent
					};
					if let Some(percent) = dialogs::show_go_to_percent_dialog(&frame_copy, current_percent) {
						let (history, history_index, path_str) = {
							let mut dm_guard = dm.lock().unwrap();
							let (history, history_index, path_str) = {
								let Some(tab) = dm_guard.active_tab_mut() else {
									return;
								};
								let target_pos = tab.session.position_from_percent(percent);
								tab.text_ctrl.set_focus();
								tab.text_ctrl.set_insertion_point(target_pos);
								tab.text_ctrl.show_position(target_pos);
								tab.session.check_and_record_history(target_pos);
								let (history, history_index) = tab.session.get_history();
								let history = history.to_vec();
								let path_str = tab.file_path.to_string_lossy().to_string();
								(history, history_index, path_str)
							};
							drop(dm_guard);
							(history, history_index, path_str)
						};
						let cfg = config.lock().unwrap();
						cfg.set_navigation_history(&path_str, &history, history_index);
					}
				}
				menu_ids::GO_BACK => {
					navigation::handle_history_navigation(&dm, &config, live_region_label, false);
				}
				menu_ids::GO_FORWARD => {
					navigation::handle_history_navigation(&dm, &config, live_region_label, true);
				}
				menu_ids::PREVIOUS_SECTION => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Section,
						false,
					);
				}
				menu_ids::NEXT_SECTION => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Section,
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(0),
						false,
					);
				}
				menu_ids::NEXT_HEADING => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(0),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_1 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(1),
						false,
					);
				}
				menu_ids::NEXT_HEADING_1 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(1),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_2 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(2),
						false,
					);
				}
				menu_ids::NEXT_HEADING_2 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(2),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_3 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(3),
						false,
					);
				}
				menu_ids::NEXT_HEADING_3 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(3),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_4 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(4),
						false,
					);
				}
				menu_ids::NEXT_HEADING_4 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(4),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_5 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(5),
						false,
					);
				}
				menu_ids::NEXT_HEADING_5 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(5),
						true,
					);
				}
				menu_ids::PREVIOUS_HEADING_6 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(6),
						false,
					);
				}
				menu_ids::NEXT_HEADING_6 => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Heading(6),
						true,
					);
				}
				menu_ids::PREVIOUS_PAGE => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Page, false);
				}
				menu_ids::NEXT_PAGE => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Page, true);
				}
				menu_ids::PREVIOUS_BOOKMARK => {
					navigation::handle_bookmark_navigation(&dm, &config, live_region_label, false, false);
				}
				menu_ids::NEXT_BOOKMARK => {
					navigation::handle_bookmark_navigation(&dm, &config, live_region_label, true, false);
				}
				menu_ids::PREVIOUS_NOTE => {
					navigation::handle_bookmark_navigation(&dm, &config, live_region_label, false, true);
				}
				menu_ids::NEXT_NOTE => {
					navigation::handle_bookmark_navigation(&dm, &config, live_region_label, true, true);
				}
				menu_ids::JUMP_TO_ALL_BOOKMARKS => {
					navigation::handle_bookmark_dialog(
						&frame_copy,
						&dm,
						&config,
						live_region_label,
						BookmarkFilterType::All,
					);
				}
				menu_ids::JUMP_TO_BOOKMARKS_ONLY => {
					navigation::handle_bookmark_dialog(
						&frame_copy,
						&dm,
						&config,
						live_region_label,
						BookmarkFilterType::BookmarksOnly,
					);
				}
				menu_ids::JUMP_TO_NOTES_ONLY => {
					navigation::handle_bookmark_dialog(
						&frame_copy,
						&dm,
						&config,
						live_region_label,
						BookmarkFilterType::NotesOnly,
					);
				}
				menu_ids::TOGGLE_BOOKMARK => {
					navigation::handle_toggle_bookmark(&dm, &config, live_region_label);
				}
				menu_ids::BOOKMARK_WITH_NOTE => {
					navigation::handle_bookmark_with_note(&frame_copy, &dm, &config, live_region_label);
				}
				menu_ids::TOGGLE_WORD_WRAP => {
					let new_state = {
						let cfg = config.lock().unwrap();
						let v = !cfg.get_app_bool("word_wrap", false);
						cfg.set_app_bool("word_wrap", v);
						cfg.flush();
						v
					};
					{
						let dm_for_wrap = Rc::clone(&dm);
						let mut dm_ref = dm.lock().unwrap();
						dm_ref.apply_word_wrap(&dm_for_wrap, new_state);
					}
					if let Some(menu_bar) = frame_copy.get_menu_bar() {
						menu_bar.check_item(menu_ids::TOGGLE_WORD_WRAP, new_state);
					}
					// TRANSLATORS: Announced when toggling word wrap; the message reflects the new state
					let msg = if new_state { t("Word wrap on.") } else { t("Word wrap off.") };
					live_region::announce(live_region_label, &msg);
					dm.lock().unwrap().restore_focus();
				}
				menu_ids::VIEW_NOTE_TEXT => {
					navigation::handle_view_note_text(&frame_copy, &dm, &config);
				}
				menu_ids::PREVIOUS_LINK => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Link, false);
				}
				menu_ids::NEXT_LINK => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Link, true);
				}
				menu_ids::PREVIOUS_IMAGE => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Image,
						false,
					);
				}
				menu_ids::NEXT_IMAGE => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Image, true);
				}
				menu_ids::PREVIOUS_FIGURE => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Figure,
						false,
					);
				}
				menu_ids::NEXT_FIGURE => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Figure,
						true,
					);
				}
				menu_ids::PREVIOUS_TABLE => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Table,
						false,
					);
				}
				menu_ids::NEXT_TABLE => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::Table, true);
				}
				menu_ids::PREVIOUS_SEPARATOR => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Separator,
						false,
					);
				}
				menu_ids::NEXT_SEPARATOR => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::Separator,
						true,
					);
				}
				menu_ids::PREVIOUS_LIST => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::List, false);
				}
				menu_ids::NEXT_LIST => {
					navigation::handle_marker_navigation(&dm, &config, live_region_label, MarkerNavTarget::List, true);
				}
				menu_ids::PREVIOUS_LIST_ITEM => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::ListItem,
						false,
					);
				}
				menu_ids::NEXT_LIST_ITEM => {
					navigation::handle_marker_navigation(
						&dm,
						&config,
						live_region_label,
						MarkerNavTarget::ListItem,
						true,
					);
				}
				menu_ids::CONTAINER_START => {
					navigation::handle_container_navigation(&dm, &config, live_region_label, false);
				}
				menu_ids::CONTAINER_END => {
					navigation::handle_container_navigation(&dm, &config, live_region_label, true);
				}
				menu_ids::EXPORT_TO_PLAIN_TEXT => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					let default_name =
						// TRANSLATORS: Fallback file name stem used when the document's path has no file stem
						tab.file_path.file_stem().map_or_else(|| t("document"), |s| s.to_string_lossy().to_string());
					let default_file = format!("{default_name}.txt");
					// TRANSLATORS: File filter shown in the "Export to plain text" save dialog
					let wildcard = t("Plain text files (*.txt)|*.txt|All files (*.*)|*.*");
					let dialog = FileDialog::builder(&frame_copy)
						// TRANSLATORS: Title of the file save dialog when exporting a document to plain text
						.with_message(&t("Export document to plain text"))
						.with_default_file(&default_file)
						.with_wildcard(&wildcard)
						.with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
						.build();
					if dialog.show_modal() == ID_OK {
						if let Some(path) = dialog.get_path() {
							if let Err(e) = tab.session.export_as(&path, paperback_core::export::ExportFormat::Text) {
								tracing::error!(path = %path, error = %e, "failed to export document as text");
								let dialog =
									// TRANSLATORS: Error dialog shown when exporting a document to another format fails
									MessageDialog::builder(&frame_copy, &t("Failed to export document."), &t("Error"))
										.with_style(
											MessageDialogStyle::OK
												| MessageDialogStyle::IconError | MessageDialogStyle::Centre,
										)
										.build();
								dialog.show_modal();
							}
						}
					}
				}
				menu_ids::EXPORT_TO_HTML => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					let default_name =
						// TRANSLATORS: Fallback file name stem used when the document's path has no file stem
						tab.file_path.file_stem().map_or_else(|| t("document"), |s| s.to_string_lossy().to_string());
					let default_file = format!("{default_name}.html");
					// TRANSLATORS: File filter shown in the "Export to HTML" save dialog
					let wildcard = t("HTML files (*.html)|*.html|All files (*.*)|*.*");
					let dialog = FileDialog::builder(&frame_copy)
						// TRANSLATORS: Title of the file save dialog when exporting a document to HTML
						.with_message(&t("Export document to HTML"))
						.with_default_file(&default_file)
						.with_wildcard(&wildcard)
						.with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
						.build();
					if dialog.show_modal() == ID_OK {
						if let Some(path) = dialog.get_path() {
							if let Err(e) = tab.session.export_as(&path, paperback_core::export::ExportFormat::Html) {
								tracing::error!(path = %path, error = %e, "failed to export document as HTML");
								let dialog =
									// TRANSLATORS: Error dialog shown when exporting a document to another format fails
									MessageDialog::builder(&frame_copy, &t("Failed to export document."), &t("Error"))
										.with_style(
											MessageDialogStyle::OK
												| MessageDialogStyle::IconError | MessageDialogStyle::Centre,
										)
										.build();
								dialog.show_modal();
							}
						}
					}
				}
				menu_ids::EXPORT_TO_MARKDOWN => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					let default_name =
						// TRANSLATORS: Fallback file name stem used when the document's path has no file stem
						tab.file_path.file_stem().map_or_else(|| t("document"), |s| s.to_string_lossy().to_string());
					let default_file = format!("{default_name}.md");
					// TRANSLATORS: File filter shown in the "Export to Markdown" save dialog
					let wildcard = t("Markdown files (*.md)|*.md|All files (*.*)|*.*");
					let dialog = FileDialog::builder(&frame_copy)
						// TRANSLATORS: Title of the file save dialog when exporting a document to Markdown
						.with_message(&t("Export document to Markdown"))
						.with_default_file(&default_file)
						.with_wildcard(&wildcard)
						.with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
						.build();
					if dialog.show_modal() == ID_OK {
						if let Some(path) = dialog.get_path() {
							if let Err(e) = tab.session.export_as(&path, paperback_core::export::ExportFormat::Markdown)
							{
								tracing::error!(path = %path, error = %e, "failed to export document as Markdown");
								let dialog =
									// TRANSLATORS: Error dialog shown when exporting a document to another format fails
									MessageDialog::builder(&frame_copy, &t("Failed to export document."), &t("Error"))
										.with_style(
											MessageDialogStyle::OK
												| MessageDialogStyle::IconError | MessageDialogStyle::Centre,
										)
										.build();
								dialog.show_modal();
							}
						}
					}
				}
				menu_ids::EXPORT_DOCUMENT_DATA => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					let default_name =
						// TRANSLATORS: Fallback file name stem used when the document's path has no file stem
						tab.file_path.file_stem().map_or_else(|| t("document"), |s| s.to_string_lossy().to_string());
					let default_file = format!("{default_name}.paperback");
					// TRANSLATORS: File filter shown in the export/import notes-and-bookmarks (.paperback) dialogs
					let wildcard = t("Paperback files (*.paperback)|*.paperback");
					let dialog = FileDialog::builder(&frame_copy)
						// TRANSLATORS: Title of the file save dialog when exporting a document's notes and bookmarks
						.with_message(&t("Export notes and bookmarks"))
						.with_default_file(&default_file)
						.with_wildcard(&wildcard)
						.with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
						.build();
					if dialog.show_modal() == ID_OK
						&& let Some(path) = dialog.get_path()
					{
						let path_str = tab.file_path.to_string_lossy();
						config.lock().unwrap().export_document_settings(&path_str, &path);
						tracing::info!(doc = %tab.file_path.display(), export = %path, "document data exported");
						let dialog = MessageDialog::builder(
							&frame_copy,
							// TRANSLATORS: Success message shown after exporting a document's notes and bookmarks
							&t("Notes and bookmarks exported successfully."),
							// TRANSLATORS: Title of the export-succeeded dialog
							&t("Export Successful"),
						)
						.with_style(
							MessageDialogStyle::OK | MessageDialogStyle::IconInformation | MessageDialogStyle::Centre,
						)
						.build();
						dialog.show_modal();
					}
				}
				menu_ids::IMPORT_DOCUMENT_DATA => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					// TRANSLATORS: File filter shown in the export/import notes-and-bookmarks (.paperback) dialogs
					let wildcard = t("Paperback files (*.paperback)|*.paperback");
					let dialog = FileDialog::builder(&frame_copy)
						// TRANSLATORS: Title of the file open dialog when importing a document's notes and bookmarks
						.with_message(&t("Import notes and bookmarks"))
						.with_wildcard(&wildcard)
						.with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
						.build();
					if dialog.show_modal() == ID_OK
						&& let Some(path) = dialog.get_path()
					{
						let path_str = tab.file_path.to_string_lossy();
						let pos = {
							let config = config.lock().unwrap();
							config.import_settings_from_file(&path_str, &path);
							let max_pos = tab.text_ctrl.get_last_position();
							config.get_validated_document_position(&path_str, max_pos)
						};
						tracing::info!(doc = %tab.file_path.display(), import = %path, "document data imported");
						if pos >= 0 {
							tab.text_ctrl.set_insertion_point(pos);
							tab.text_ctrl.show_position(pos);
						}
						let dialog = MessageDialog::builder(
							&frame_copy,
							// TRANSLATORS: Success message shown after importing a document's notes and bookmarks
							&t("Notes and bookmarks imported successfully."),
							// TRANSLATORS: Title of the import-succeeded dialog
							&t("Import Successful"),
						)
						.with_style(
							MessageDialogStyle::OK | MessageDialogStyle::IconInformation | MessageDialogStyle::Centre,
						)
						.build();
						dialog.show_modal();
					}
				}
				menu_ids::WORD_COUNT => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					if let Some(tab) = dm_ref.active_tab() {
						let selection = tab.text_ctrl.get_string_selection();
						let (word_count, is_selection) = if selection.trim().is_empty() {
							(tab.session.stats().word_count, false)
						} else {
							(paperback_core::document::DocumentStats::from_text(&selection).word_count, true)
						};
						let wpm = config.lock().unwrap().get_app_int("reading_speed_wpm", 150);
						dialogs::show_word_count_dialog(&frame_copy, word_count, wpm, is_selection);
					}
				}
				menu_ids::DOCUMENT_INFO => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					if let Some(tab) = dm_ref.active_tab() {
						let stats = tab.session.stats();
						let title = tab.session.title();
						let author = tab.session.author();
						dialogs::show_document_info_dialog(&frame_copy, &tab.file_path, &title, &author, stats);
					}
				}
				menu_ids::TABLE_OF_CONTENTS => {
					let mut dm_guard = dm.lock().unwrap();
					if let Some(tab) = dm_guard.active_tab_mut() {
						let toc_items = &tab.session.handle().document().toc_items;
						if toc_items.is_empty() {
							// TRANSLATORS: Announced when opening the Table of Contents for a document that has none
							live_region::announce(live_region_label, &t("No table of contents."));
							return;
						}
						let current_pos = tab.text_ctrl.get_insertion_point();
						let current_pos_usize = usize::try_from(current_pos).unwrap_or(0);
						let current_toc_offset = tab.session.handle().find_closest_toc_offset(current_pos_usize);
						if let Some(offset) = dialogs::show_toc_dialog(
							&frame_copy,
							toc_items,
							i32::try_from(current_toc_offset).unwrap_or(i32::MAX),
						) {
							tab.text_ctrl.set_focus();
							tab.text_ctrl.set_insertion_point(i64::from(offset));
							tab.text_ctrl.show_position(i64::from(offset));
							tab.session.check_and_record_history(i64::from(offset));
							let (history, history_index) = tab.session.get_history();
							let path_str = tab.file_path.to_string_lossy();
							let cfg = config.lock().unwrap();
							cfg.set_navigation_history(&path_str, history, history_index);
						}
					}
				}
				menu_ids::ELEMENTS_LIST => {
					let mut dm_guard = dm.lock().unwrap();
					if let Some(tab) = dm_guard.active_tab_mut() {
						let current_pos = tab.text_ctrl.get_insertion_point();
						if let Some(offset) = dialogs::show_elements_dialog(&frame_copy, &tab.session, current_pos) {
							tab.text_ctrl.set_focus();
							tab.text_ctrl.set_insertion_point(offset);
							tab.text_ctrl.show_position(offset);
							tab.session.check_and_record_history(offset);
							let (history, history_index) = tab.session.get_history();
							let path_str = tab.file_path.to_string_lossy();
							let cfg = config.lock().unwrap();
							cfg.set_navigation_history(&path_str, history, history_index);
						}
					}
				}
				menu_ids::OPEN_IN_WEB_VIEW => {
					let Ok(dm_ref) = dm.try_lock() else {
						return;
					};
					let Some(tab) = dm_ref.active_tab() else {
						return;
					};
					let current_pos = tab.text_ctrl.get_insertion_point();
					let temp_dir = env::temp_dir().to_string_lossy().to_string();
					if let Some(target) = tab.session.webview_target_path(current_pos, &temp_dir) {
						let mut url = format!("file:///{}", target.path.replace('\\', "/"));
						let fragment =
							target.fragment.or_else(|| tab.session.webview_fragment_for_position(current_pos));
						if let Some(fragment) = fragment {
							url.push('#');
							url.push_str(&fragment);
						}
						drop(dm_ref);
						dialogs::show_web_view_dialog(
							&frame_copy,
							// TRANSLATORS: Title of the window that renders a document's content as HTML (e.g. for embedded web pages)
							&t("Web View"),
							&url,
							true,
							Some(Box::new(|url| {
								if url.to_lowercase().starts_with("http://")
									|| url.to_lowercase().starts_with("https://")
									|| url.to_lowercase().starts_with("mailto:")
								{
									launch_default_browser(url, BrowserLaunchFlags::Default);
									false
								} else {
									true
								}
							})),
						);
					} else {
						tracing::warn!(path = %tab.file_path.display(), "could not determine web view content");
						let dialog = MessageDialog::builder(
							&frame_copy,
							// TRANSLATORS: Error shown when the document has no content that can be rendered in the Web View
							&t("Could not determine content to display in Web View."),
							&t("Error"),
						)
						.with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError | MessageDialogStyle::Centre)
						.build();
						dialog.show_modal();
					}
				}
				menu_ids::REVEAL_FILE_IN_FOLDER => {
					help::handle_reveal_file_in_folder(&frame_copy, &dm);
				}
				menu_ids::VIEW_SOURCE => {
					// `None` => format has no text source; `Some(None)` => source could not
					// be loaded; `Some(Some(..))` => source ready. Locks are dropped before
					// any dialog is shown.
					let outcome: Option<Option<(paperback_core::session::SourceView, String)>> = {
						let Ok(dm_ref) = dm.try_lock() else {
							return;
						};
						let Some(tab) = dm_ref.active_tab() else {
							return;
						};
						if tab.session.source_view_available() {
							let current_pos = tab.text_ctrl.get_insertion_point();
							let orig_name = tab
								.file_path
								.file_name()
								// TRANSLATORS: Fallback file name stem used when the document's path has no file stem
								.map_or_else(|| t("document"), |name| name.to_string_lossy().to_string());
							let temp_dir = env::temp_dir().to_string_lossy().to_string();
							Some(tab.session.view_source(current_pos, &temp_dir).map(|view| (view, orig_name)))
						} else {
							None
						}
					};
					match outcome {
						Some(Some((view, orig_name))) => {
							// TRANSLATORS: Prefix before the file name in the tab title for a "View Source" tab, e.g. "Source: book.epub"
							let title = format!("{} {orig_name}", t("Source:"));
							let window = super::app::main_window_from_ptr()
								.expect("menu events only fire after the app is built");
							window.request_open(
								Path::new(&view.path),
								OpenRequest {
									track: false,
									title_override: Some(title),
									initial_caret: Some(view.caret),
									..OpenRequest::default()
								},
							);
						}
						unavailable => {
							let message = if unavailable.is_none() {
								tracing::debug!("source view not available for this format");
								// TRANSLATORS: Error shown when "View Source" is used on a document format that has no raw source to view
								t("Source view is not available for this document format.")
							} else {
								tracing::warn!("failed to load document source for view source");
								// TRANSLATORS: Error shown when "View Source" fails to load the document's underlying source
								t("Could not load the document source.")
							};
							let dialog = MessageDialog::builder(&frame_copy, &message, &t("Error"))
								.with_style(
									MessageDialogStyle::OK | MessageDialogStyle::IconError | MessageDialogStyle::Centre,
								)
								.build();
							dialog.show_modal();
						}
					}
				}
				menu_ids::OPTIONS | menu_ids::PREFERENCES => {
					let current_language = TranslationManager::instance().lock().unwrap().current_language();
					let options = {
						let cfg = config.lock().unwrap();
						dialogs::show_options_dialog(&frame_copy, &cfg)
					};
					let Some(options) = options else {
						return;
					};
					let (
						old_word_wrap,
						old_render_tables_inline,
						old_compact_menu,
						old_readability_font,
						old_line_spacing,
						old_bg_color,
						old_text_alignment,
						old_letter_spacing,
						old_paragraph_spacing,
					) = {
						let cfg = config.lock().unwrap();
						(
							cfg.get_app_bool("word_wrap", false),
							cfg.get_app_bool("render_tables_inline", true),
							cfg.get_app_bool("compact_go_menu", true),
							cfg.get_readability_font(),
							cfg.get_line_spacing(),
							cfg.get_bg_color(),
							cfg.get_text_alignment(),
							cfg.get_letter_spacing(),
							cfg.get_paragraph_spacing(),
						)
					};
					let cfg = config.lock().unwrap();
					cfg.set_app_bool("restore_previous_documents", options.restore_previous_documents);
					cfg.set_app_bool("word_wrap", options.word_wrap);
					cfg.set_app_bool("render_tables_inline", options.render_tables_inline);
					cfg.set_app_bool("minimize_to_tray", options.minimize_to_tray);
					cfg.set_app_bool("start_maximized", options.start_maximized);
					cfg.set_app_bool("compact_go_menu", options.compact_go_menu);
					cfg.set_app_bool("navigation_wrap", options.navigation_wrap);
					cfg.set_app_bool("check_for_updates_on_startup", options.check_for_updates_on_startup);
					cfg.set_app_bool("bookmark_sounds", options.bookmark_sounds);
					cfg.set_app_int("recent_documents_to_show", options.recent_documents_to_show);
					cfg.set_app_int("reading_speed_wpm", options.reading_speed_wpm);
					cfg.set_app_string("language", &options.language);
					set_update_channel(&cfg, options.update_channel);
					cfg.set_hotkey(&options.hotkey);
					cfg.set_readability_font(&options.readability_font);
					cfg.set_line_spacing(options.line_spacing);
					cfg.set_bg_color(options.bg_color);
					cfg.set_text_alignment(options.text_alignment);
					cfg.set_letter_spacing(options.letter_spacing);
					cfg.set_paragraph_spacing(options.paragraph_spacing);
					cfg.flush();
					tracing::info!("settings saved");
					#[cfg(target_os = "windows")]
					{
						re_register_hotkey(&hotkey_handle_for_options, &options.hotkey);
					}
					drop(cfg);
					let options_word_wrap = options.word_wrap;
					let options_render_tables_inline = options.render_tables_inline;
					let render_tables_inline_changed = old_render_tables_inline != options_render_tables_inline;
					let font_changed = old_readability_font != options.readability_font;
					let line_spacing_changed = old_line_spacing != options.line_spacing;
					let bg_color_changed = old_bg_color != options.bg_color;
					let text_alignment_changed = old_text_alignment != options.text_alignment;
					let letter_spacing_changed = old_letter_spacing != options.letter_spacing;
					let paragraph_spacing_changed = old_paragraph_spacing != options.paragraph_spacing;
					let needs_rebuild = old_word_wrap != options_word_wrap
						|| (font_changed && build_font_from_readability(&options.readability_font).is_none())
						|| (bg_color_changed && options.bg_color < 0)
						|| (font_changed && options.readability_font.color < 0);
					if needs_rebuild {
						let dm_for_wrap = Rc::clone(&dm);
						let mut dm_ref = dm.lock().unwrap();
						dm_ref.apply_word_wrap(&dm_for_wrap, options_word_wrap);
						dm_ref.restore_focus();
					} else {
						let dm_ref = dm.lock().unwrap();
						if font_changed {
							if let Some(font) = build_font_from_readability(&options.readability_font) {
								dm_ref.apply_font(&font);
							}
							dm_ref.apply_color(options.readability_font.color);
						}
						if bg_color_changed {
							dm_ref.apply_bg_color(options.bg_color);
						}
						if line_spacing_changed {
							dm_ref.apply_line_spacing(options.line_spacing);
						}
						if text_alignment_changed {
							dm_ref.apply_text_alignment(options.text_alignment);
						}
						if letter_spacing_changed {
							dm_ref.apply_letter_spacing(options.letter_spacing);
						}
						if paragraph_spacing_changed {
							dm_ref.apply_paragraph_spacing(options.paragraph_spacing);
						}
					}
					if render_tables_inline_changed {
						let window =
							super::app::main_window_from_ptr().expect("menu events only fire after the app is built");
						window.request_render_tables_reparse(options_render_tables_inline);
					}
					let options_compact_menu = options.compact_go_menu;
					if current_language != options.language || old_compact_menu != options_compact_menu {
						if current_language != options.language {
							let _ = TranslationManager::instance().lock().unwrap().set_language(&options.language);
						}
						let dm_ref = dm.lock().unwrap();
						update_title_from_manager(&frame_copy, &dm_ref);
					}
					let menu_bar = menu::create_menu_bar(&config.lock().unwrap());
					frame_copy.set_menu_bar(menu_bar);
					let dm_ref = dm.lock().unwrap();
					let has_docs = dm_ref.tab_count() > 0;
					let has_reopen = dm_ref.has_recently_closed();
					drop(dm_ref);
					menu::update_menu_item_states(&frame_copy, has_docs);
					menu::update_reopen_state(&frame_copy, has_reopen);
				}
				menu_ids::SLEEP_TIMER => {
					if sleep_timer_running_for_menu.get() {
						sleep_timer_for_menu.stop();
						sleep_timer_running_for_menu.set(false);
						sleep_timer_start_for_menu.set(0);
						sleep_timer_duration_for_menu.set(0);
						SLEEP_TIMER_START_MS.store(0, Ordering::SeqCst);
						SLEEP_TIMER_DURATION_MINUTES.store(0, Ordering::SeqCst);
						tracing::info!("sleep timer cancelled");
						let dm_ref = dm.lock().unwrap();
						update_title_from_manager(&frame_copy, &dm_ref);
						// TRANSLATORS: Announced when the user cancels a running sleep timer
						live_region::announce(live_region_label, &t("Sleep timer cancelled."));
						return;
					}
					let initial_duration = config.lock().unwrap().get_app_int("sleep_timer_duration", 30);
					if let Some(duration) = dialogs::show_sleep_timer_dialog(&frame_copy, initial_duration) {
						{
							let cfg = config.lock().unwrap();
							cfg.set_app_int("sleep_timer_duration", duration);
							cfg.flush();
						}
						let duration_ms = u64::try_from(duration).unwrap_or(0) * 60 * 1000;
						sleep_timer_for_menu.start(i32::try_from(duration_ms).unwrap_or(i32::MAX), true);
						sleep_timer_running_for_menu.set(true);
						tracing::info!(duration_minutes = duration, "sleep timer started");
						let now = SystemTime::now()
							.duration_since(UNIX_EPOCH)
							.ok()
							.and_then(|d| i64::try_from(d.as_millis()).ok())
							.unwrap_or(0);
						sleep_timer_start_for_menu.set(now);
						sleep_timer_duration_for_menu.set(duration);
						SLEEP_TIMER_START_MS.store(now, Ordering::SeqCst);
						SLEEP_TIMER_DURATION_MINUTES.store(duration, Ordering::SeqCst);
						let msg = if duration == 1 {
							// TRANSLATORS: Announcement when the sleep timer is set for exactly 1 minute
							t("Sleep timer set for 1 minute.")
						} else {
							// TRANSLATORS: Announcement when the sleep timer is set; %d is the number of minutes (always 2 or more)
							t("Sleep timer set for %d minutes.").replace("%d", &duration.to_string())
						};
						live_region::announce(live_region_label, &msg);
					}
				}
				menu_ids::ABOUT => {
					dialogs::show_about_dialog(&frame_copy);
				}
				menu_ids::VIEW_HELP_BROWSER => {
					help::handle_view_help_browser(&frame_copy);
				}
				menu_ids::VIEW_HELP_PAPERBACK => {
					help::handle_view_help_paperback(&frame_copy);
				}
				menu_ids::CHECK_FOR_UPDATES => {
					let channel = get_update_channel(&config.lock().unwrap());
					help::run_update_check(false, channel);
				}
				menu_ids::DONATE => {
					help::handle_donate(&frame_copy);
				}
				_ => {
					if (menu_ids::RECENT_DOCUMENT_BASE..=menu_ids::RECENT_DOCUMENT_MAX).contains(&id) {
						let doc_index = id - menu_ids::RECENT_DOCUMENT_BASE;
						let recent_docs = {
							let config_guard = config.lock().unwrap();
							menu::recent_documents_for_menu(&config_guard)
						};
						if let Ok(doc_index) = usize::try_from(doc_index)
							&& let Some(path) = recent_docs.get(doc_index)
						{
							let window = super::app::main_window_from_ptr()
								.expect("menu events only fire after the app is built");
							window.request_open(Path::new(path), OpenRequest::default());
						}
					} else if id == menu_ids::SHOW_ALL_DOCUMENTS {
						let has_documents = {
							let config_guard = config.lock().unwrap();
							!config_guard.get_all_documents().is_empty()
						};
						if !has_documents {
							// TRANSLATORS: Announced when opening "All Documents" while the recent-documents list is empty
							live_region::announce(live_region_label, &t("No recent documents."));
							return;
						}
						let open_paths = dm.lock().unwrap().open_paths();
						let config_for_dialog = Rc::clone(&config);
						let result = dialogs::show_all_documents_dialog(&frame_copy, &config_for_dialog, open_paths);
						{
							let mut dm_ref = dm.lock().unwrap();
							for path_str in &result.paths_to_close {
								let path = Path::new(path_str);
								if let Some(index) = dm_ref.find_tab_by_path(path) {
									dm_ref.close_document(index, false);
								}
							}
							// Runs after the close loop: close_document pushes the closed
							// tab onto the reopen stack, and a removed document must not
							// stay reopenable.
							dm_ref.forget_recently_closed(&result.paths_removed);
							if !result.paths_to_close.is_empty() {
								update_title_from_manager(&frame_copy, &dm_ref);
								dm_ref.restore_focus();
							}
						}
						if let Some(path) = result.open {
							let window = super::app::main_window_from_ptr()
								.expect("menu events only fire after the app is built");
							window.request_open(Path::new(&path), OpenRequest::default());
						}
						// The dialog closes tabs as well as opening one, so the menus need
						// refreshing even when nothing was opened.
						let menu_bar = menu::create_menu_bar(&config.lock().unwrap());
						frame_copy.set_menu_bar(menu_bar);
						let dm_ref = dm.lock().unwrap();
						let has_docs = dm_ref.tab_count() > 0;
						let has_reopen = dm_ref.has_recently_closed();
						drop(dm_ref);
						menu::update_menu_item_states(&frame_copy, has_docs);
						menu::update_reopen_state(&frame_copy, has_reopen);
					}
				}
			}
		});
	}
}

fn spawn_parse(seq: u64, path: String, password: String, forced_extension: String, render_tables_inline: bool) {
	std::thread::spawn(move || {
		let result = DocumentSession::new(&path, &password, &forced_extension, render_tables_inline);
		post_completion(move |window| window.finish_parse(seq, result));
	});
}

/// Posts a parse result to the UI thread. The `Send` closure resolves the non-`Send` window only
/// after reaching that thread, then `run_or_defer` waits out any lock-holding modal.
fn post_completion(completion: impl FnOnce(&'static MainWindow) + Send + 'static) {
	call_after(Box::new(move || {
		if SHUTTING_DOWN.load(Ordering::SeqCst) {
			return;
		}
		let Some(window) = super::app::main_window_from_ptr() else {
			return;
		};
		window.run_or_defer(Box::new(move || completion(window)));
	}));
	wake_up_idle();
}

/// Close the active document, announcing the newly focused document for screen readers.
///
/// The `set_selection` inside `close_document` fires `on_page_changing` while the
/// caller holds the manager lock, so the generic switch announcement is suppressed
/// and this function announces the new focus itself instead, before the focus change
/// actually happens.
fn close_active_document_announced(dm: &mut DocumentManager, live_region_label: StaticText) {
	let Some(index) = dm.active_tab_index() else {
		return;
	};
	let next = dm.active_index_after_closing(index).and_then(|i| dm.get_tab(i)).map(display_title);
	if let Some(next) = &next {
		live_region::announce(live_region_label, next);
	}
	dm.close_document(index, true);
}

/// Refreshes the window title and status bar to match the active document. This is the single
/// place either is derived from manager state, so every open, close, and tab switch agrees on
/// what they say.
fn update_title_from_manager(frame: &Frame, dm: &DocumentManager) {
	if let Some(tab) = dm.active_tab() {
		// TRANSLATORS: Window title when a document is open; {} is the document title
		frame.set_title(&t("Paperback - {}").replace("{}", &display_title(tab)));
	} else {
		// TRANSLATORS: Main window title when no document is open
		frame.set_title(&t("Paperback"));
	}
	// Keep loading or reloading status visible until all background work finishes.
	let mut status_text = dm.parses().busy_status_text().unwrap_or_else(|| {
		dm.active_tab().map_or_else(
			// TRANSLATORS: Default status bar text when no document is open
			|| t("Ready"),
			|tab| status::format_status_text(&tab.session.get_status_info(tab.text_ctrl.get_insertion_point())),
		)
	});
	let sleep_start = SLEEP_TIMER_START_MS.load(Ordering::SeqCst);
	if sleep_start > 0 {
		let sleep_duration = SLEEP_TIMER_DURATION_MINUTES.load(Ordering::SeqCst);
		let remaining = status::calculate_sleep_timer_remaining(sleep_start, sleep_duration);
		if remaining > 0 {
			status_text = status::format_sleep_timer_status(&status_text, remaining);
		}
	}
	frame.set_status_text(&status_text, 0);
}

#[cfg(target_os = "windows")]
pub struct HotkeyHandle {
	pub(crate) thread_id: u32,
	pub(crate) join_handle: std::thread::JoinHandle<()>,
}

#[cfg(target_os = "windows")]
pub fn start_hotkey_listener(hotkey: &paperback_core::config::HotkeyConfig) -> Option<HotkeyHandle> {
	use windows::Win32::{
		System::Threading::GetCurrentThreadId,
		UI::{
			Input::KeyboardAndMouse::{
				HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
			},
			WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY},
		},
	};
	const HOTKEY_ID: i32 = 1;
	let mut modifiers = HOT_KEY_MODIFIERS(0);
	if hotkey.ctrl {
		modifiers |= MOD_CONTROL;
	}
	if hotkey.alt {
		modifiers |= MOD_ALT;
	}
	if hotkey.shift {
		modifiers |= MOD_SHIFT;
	}
	if hotkey.win {
		modifiers |= MOD_WIN;
	}
	let vk = char_to_vk(hotkey.key)?;
	let (thread_id_tx, thread_id_rx) = std::sync::mpsc::channel();
	let join_handle = std::thread::spawn(move || {
		let thread_id = unsafe { GetCurrentThreadId() };
		let _ = thread_id_tx.send(thread_id);
		let registered = unsafe { RegisterHotKey(None, HOTKEY_ID, modifiers, vk).is_ok() };
		if !registered {
			return;
		}
		let mut msg = MSG::default();
		loop {
			let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
			if result.0 <= 0 {
				break;
			}
			if msg.message == WM_HOTKEY {
				call_after(Box::new(|| {
					if let Some(window) = super::app::main_window_from_ptr() {
						window.handle_ipc_command(IpcCommand::ToggleVisibility);
					}
				}));
				wake_up_idle();
			}
		}
		unsafe {
			let _ = UnregisterHotKey(None, HOTKEY_ID);
		}
	});
	let thread_id = thread_id_rx.recv().ok()?;
	Some(HotkeyHandle { thread_id, join_handle })
}

#[cfg(target_os = "windows")]
fn char_to_vk(ch: char) -> Option<u32> {
	if ch == '\0' {
		return None;
	}
	use windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
	let code = u16::try_from(u32::from(ch)).ok()?;
	let result = unsafe { VkKeyScanW(code) };
	let low_byte = u8::try_from(result & 0xFF).ok()?;
	if low_byte == 0xFF { None } else { Some(u32::from(low_byte)) }
}

#[cfg(target_os = "windows")]
fn re_register_hotkey(
	hotkey_handle: &Rc<RefCell<Option<HotkeyHandle>>>,
	hotkey: &paperback_core::config::HotkeyConfig,
) {
	use windows::Win32::{
		Foundation::{LPARAM, WPARAM},
		UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT},
	};
	if let Some(handle) = hotkey_handle.borrow_mut().take() {
		if handle.thread_id != 0 {
			unsafe {
				let _ = PostThreadMessageW(handle.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
			}
		}
		let _ = handle.join_handle.join();
	}
	*hotkey_handle.borrow_mut() = start_hotkey_listener(hotkey);
}
