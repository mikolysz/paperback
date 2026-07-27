use std::{path::Path, rc::Rc, sync::Mutex};

use paperback_core::{config::ConfigManager, parser::parser_supports_extension};
use patois::t;
use wxdragon::prelude::*;

use super::DIALOG_PADDING;

/// Ensures some parser can handle `path`, asking the user how to open it when its extension is
/// unrecognized and remembering that choice for future opens. Returns `false` when the user
/// cancels the prompt or picks a format this build cannot parse.
pub fn ensure_parser_ready_for_path(parent: &Frame, path: &Path, config: &Rc<Mutex<ConfigManager>>) -> bool {
	let extension = parser_extension_for_path(path);
	if extension.is_empty() || parser_supports_extension(&extension) {
		return true;
	}
	// Hold the config lock only for these short reads and writes. Keeping a guard alive across the
	// dialogs below risks deadlock: their nested event loops dispatch other handlers, which may
	// need the same non-reentrant lock.
	let path_str = path.to_string_lossy();
	let saved_format = config.lock().unwrap().get_document_format(&path_str);
	if !saved_format.is_empty() && parser_supports_extension(&saved_format) {
		return true;
	}
	let Some(format) = show_open_as_dialog(parent, path) else {
		return false;
	};
	if !parser_supports_extension(&format) {
		// TRANSLATORS: Error shown when the user picks a file format from the "Open As" dialog that this parser build doesn't support
		let message = t("Unsupported format selected.");
		let title = t("Error");
		let dialog = MessageDialog::builder(parent, &message, &title)
			.with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError | MessageDialogStyle::Centre)
			.build();
		dialog.show_modal();
		return false;
	}
	config.lock().unwrap().set_document_format(&path_str, &format);
	true
}

fn parser_extension_for_path(path: &Path) -> String {
	let from_path = path.extension().and_then(|ext| ext.to_str()).map(clean_extension_token).unwrap_or_default();
	if !from_path.is_empty() {
		return from_path;
	}
	// Fallback for odd IPC/CLI strings that may contain trailing quotes or whitespace.
	let raw = path.to_string_lossy();
	let cleaned = raw.trim().trim_matches(['"', '\'', '\0']);
	let candidate = cleaned
		.rsplit_once(['/', '\\'])
		.map_or(cleaned, |(_, file_name)| file_name)
		.rsplit_once('.')
		.map_or("", |(_, ext)| ext)
		.trim();
	clean_extension_token(candidate)
}

fn clean_extension_token(raw: &str) -> String {
	let trimmed = raw.trim().trim_matches(['"', '\'', '\0']);
	trimmed.chars().take_while(char::is_ascii_alphanumeric).collect()
}

fn show_open_as_dialog(parent: &Frame, path: &Path) -> Option<String> {
	// TRANSLATORS: Title of the Open As dialog
	let title = t("Open As");
	let dialog = Dialog::builder(parent, &title).build();
	// TRANSLATORS: Prompt template informing the user that no parser was found for their file. The {} placeholder is replaced with the file path.
	let message_template = t("No suitable parser was found for {}.\nHow would you like to open this file?");
	let message = message_template.replace("{}", &path.display().to_string());
	let label = StaticText::builder(&dialog).with_label(&message).build();
	// TRANSLATORS: Label for the format selection dropdown
	let format_label_text = t("Open &as:");
	let format_label = StaticText::builder(&dialog).with_label(&format_label_text).build();
	let format_combo = Choice::builder(&dialog).build();
	// TRANSLATORS: Choice option to open a file as plain text
	format_combo.append(&t("Plain Text"));
	// TRANSLATORS: Choice option to open a file as HTML
	format_combo.append(&t("HTML"));
	// TRANSLATORS: Choice option to open a file as Markdown
	format_combo.append(&t("Markdown"));
	format_combo.set_selection(0);
	#[cfg(target_os = "macos")]
	format_combo.set_accessibility_label(format_label_text.replace('&', "").trim_end_matches(':').trim());
	// TRANSLATORS: Label for the confirmation button
	let ok_label = t("OK");
	let ok_button = Button::builder(&dialog).with_label(&ok_label).build();
	// TRANSLATORS: Label for the cancellation button
	let cancel_label = t("Cancel");
	let cancel_button = Button::builder(&dialog).with_id(ID_CANCEL).with_label(&cancel_label).build();
	let dialog_for_ok = dialog;
	ok_button.on_click(move |_| {
		dialog_for_ok.end_modal(ID_OK);
	});
	let dialog_for_cancel = dialog;
	cancel_button.on_click(move |_| {
		dialog_for_cancel.end_modal(ID_CANCEL);
	});
	let content_sizer = BoxSizer::builder(Orientation::Vertical).build();
	content_sizer.add(&label, 0, SizerFlag::All, DIALOG_PADDING / 2);
	let format_sizer = BoxSizer::builder(Orientation::Horizontal).build();
	format_sizer.add(&format_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::Right, DIALOG_PADDING);
	format_sizer.add(&format_combo, 1, SizerFlag::Expand, 0);
	content_sizer.add_sizer(&format_sizer, 0, SizerFlag::Expand | SizerFlag::All, DIALOG_PADDING / 2);
	let button_sizer = BoxSizer::builder(Orientation::Horizontal).build();
	button_sizer.add_stretch_spacer(1);
	button_sizer.add(&ok_button, 0, SizerFlag::All, DIALOG_PADDING);
	button_sizer.add(&cancel_button, 0, SizerFlag::All, DIALOG_PADDING);
	content_sizer.add_sizer(&button_sizer, 0, SizerFlag::Expand, 0);
	dialog.set_sizer_and_fit(content_sizer, true);
	dialog.centre();
	format_combo.set_focus();
	if dialog.show_modal() != ID_OK {
		return None;
	}
	let selection = format_combo.get_selection();
	let format = match selection {
		Some(1) => "html",
		Some(2) => "md",
		_ => "txt",
	};
	Some(format.to_string())
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::parser_extension_for_path;

	#[test]
	fn parser_extension_for_path_handles_normal_paths() {
		assert_eq!(parser_extension_for_path(Path::new("book.epub")), "epub");
		assert_eq!(parser_extension_for_path(Path::new("C:\\docs\\book.PDF")), "PDF");
	}

	#[test]
	fn parser_extension_for_path_strips_quotes_and_whitespace() {
		assert_eq!(parser_extension_for_path(Path::new("  \"book.epub\"  ")), "epub");
		assert_eq!(parser_extension_for_path(Path::new("'book.txt'")), "txt");
	}

	#[test]
	fn parser_extension_for_path_returns_empty_for_no_extension() {
		assert_eq!(parser_extension_for_path(Path::new("README")), "");
	}

	#[test]
	fn parser_extension_for_path_handles_ipc_artifacts() {
		assert_eq!(parser_extension_for_path(Path::new("book.epub\u{0}")), "epub");
		assert_eq!(parser_extension_for_path(Path::new(" \"book.epub\u{0}\" ")), "epub");
	}
}
