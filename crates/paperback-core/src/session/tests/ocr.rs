//! Locating and replacing the image-only page placeholders the PDF parser leaves behind.

use super::*;
use crate::{ocr::image_only_placeholder, util::text::display_len};

/// Two pages of text with an image-only page between them, shaped the way the PDF parser builds
/// one: a page break at the start of each page, and an `ImageOnlyPage` marker sharing the
/// placeholder line's start.
fn session_with_image_only_page() -> DocumentSession {
	let placeholder = image_only_placeholder();
	// The wording depends on the platform's OCR support and can be translated.
	let page_three_offset = 9 + display_len(&placeholder) + 1;
	let content = format!("page one\n{placeholder}\npage three\n");
	let mut buffer = DocumentBuffer::with_content(content);
	buffer.add_marker(Marker::new(MarkerType::PageBreak, 0));
	buffer.add_marker(Marker::new(MarkerType::PageBreak, 9));
	buffer.add_marker(Marker::new(MarkerType::ImageOnlyPage, 9));
	buffer.add_marker(Marker::new(MarkerType::PageBreak, page_three_offset));
	session_from_buffer(buffer)
}

#[test]
fn line_bounds_at_and_line_text_at_return_the_containing_line() {
	let session = session_with_content("aaa\nbbb\nccc");
	// (start, end) are display units; end excludes the line's trailing newline.
	assert_eq!(session.line_bounds_at(0), Some((0, 3)));
	assert_eq!(session.line_text_at(0), "aaa");
	assert_eq!(session.line_bounds_at(5), Some((4, 7)));
	assert_eq!(session.line_text_at(5), "bbb");
	assert_eq!(session.line_bounds_at(11), Some((8, 11)));
	assert_eq!(session.line_text_at(11), "ccc");
}

#[test]
fn image_only_pages_reports_the_page_number_and_offset() {
	let session = session_with_image_only_page();
	assert_eq!(session.image_only_pages(), vec![(2, 9)]);
}

#[test]
fn image_only_page_at_matches_anywhere_on_the_placeholder_line() {
	let session = session_with_image_only_page();
	let page_three_offset = session.page_offset(3);
	// Include every caret position, from the line's start through its trailing newline.
	for position in 9..page_three_offset {
		assert_eq!(session.image_only_page_at(position), Some(9), "caret at {position}");
	}
	// The lines either side are ordinary text.
	assert_eq!(session.image_only_page_at(0), None);
	assert_eq!(session.image_only_page_at(8), None);
	assert_eq!(session.image_only_page_at(page_three_offset), None);
	assert_eq!(session.image_only_page_at(page_three_offset + 2), None);
}

#[test]
fn replace_image_only_pages_swaps_the_line_and_clears_the_marker() {
	let mut session = session_with_image_only_page();
	let page_three_offset = session.page_offset(3);
	assert_eq!(session.line_text_at(page_three_offset), "page three");
	let outcome = session.replace_image_only_pages(&[(9, "recognized text".to_string())]);
	assert_eq!(session.line_text_at(9), "recognized text");
	// The marker is gone, so the page is not offered for OCR a second time.
	assert!(session.image_only_pages().is_empty());
	assert_eq!(session.image_only_page_at(9), None);
	// The page after it moved by the length difference, and page navigation follows.
	assert_eq!(outcome.total_delta, 25 - page_three_offset);
	assert_eq!(session.page_offset(3), 25); // "page one\nrecognized text\n"
	assert_eq!(session.line_text_at(session.page_offset(3)), "page three");
}

#[test]
fn replace_image_only_pages_ignores_offsets_that_are_not_placeholders() {
	let mut session = session_with_image_only_page();
	let outcome = session.replace_image_only_pages(&[(0, "not a placeholder".to_string())]);
	assert_eq!(outcome.total_delta, 0);
	assert_eq!(session.line_text_at(0), "page one");
	assert_eq!(session.image_only_pages(), vec![(2, 9)]);
}
