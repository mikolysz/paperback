//! Plain-text and regex search over the rendered document, with display-unit offset conversion
//! (see [`crate::util::text::display_len`]) and wrap-around retry.

use bitflags::bitflags;
use regex::{Regex, RegexBuilder};

use crate::{
	types as ffi,
	util::text::{ch_width, display_len},
};

bitflags! {
	#[derive(Copy, Clone)]
	pub struct SearchOptions: u8 {
		const FORWARD = 1 << 0;
		const MATCH_CASE = 1 << 1;
		const WHOLE_WORD = 1 << 2;
		const REGEX = 1 << 3;
	}
}

fn display_to_byte_index(s: &str, display_idx: usize) -> usize {
	let mut display_count = 0usize;
	for (byte_idx, ch) in s.char_indices() {
		if display_count >= display_idx {
			return byte_idx;
		}
		display_count += ch_width(ch);
	}
	s.len()
}

fn byte_to_display_index(s: &str, byte_idx: usize) -> usize {
	let mut display_count = 0usize;
	for (idx, ch) in s.char_indices() {
		if idx >= byte_idx {
			break;
		}
		display_count += ch_width(ch);
	}
	display_count
}

/// The display-unit index of each byte offset in `sorted_byte_offsets` (which must be ascending),
/// computed in a single forward pass so converting many offsets stays linear in the text length
/// plus the number of offsets rather than quadratic.
fn display_offsets_at(s: &str, sorted_byte_offsets: &[usize]) -> Vec<usize> {
	let mut out = Vec::with_capacity(sorted_byte_offsets.len());
	let mut chars = s.char_indices();
	let mut byte_cursor = 0usize;
	let mut display_cursor = 0usize;
	for &target in sorted_byte_offsets {
		while byte_cursor < target {
			if let Some((byte, ch)) = chars.next() {
				byte_cursor = byte + ch.len_utf8();
				display_cursor += ch_width(ch);
			} else {
				byte_cursor = s.len();
				break;
			}
		}
		out.push(display_cursor);
	}
	out
}

/// The matcher both `reader_search` and `reader_search_all` run, honouring `options`: the needle
/// is escaped unless it is already a regex, wrapped in `\b…\b` for whole-word search, and matched
/// case-insensitively unless `MATCH_CASE` is set. `None` for an invalid regex.
fn build_matcher(needle: &str, options: SearchOptions) -> Option<Regex> {
	let escaped_needle =
		if options.contains(SearchOptions::REGEX) { needle.to_string() } else { regex::escape(needle) };
	let pattern =
		if options.contains(SearchOptions::WHOLE_WORD) { format!(r"\b{escaped_needle}\b") } else { escaped_needle };
	let mut builder = RegexBuilder::new(&pattern);
	if !options.contains(SearchOptions::MATCH_CASE) {
		builder.case_insensitive(true);
	}
	builder.build().ok()
}

#[must_use]
pub fn reader_search(haystack: &str, needle: &str, start: i64, options: SearchOptions) -> i64 {
	if needle.is_empty() {
		return -1;
	}
	let start_display = usize::try_from(start.clamp(0, i64::MAX)).unwrap_or(0);
	let start_byte = display_to_byte_index(haystack, start_display);
	// Build regex for search - this avoids copying/lowercasing the entire haystack.
	let Some(re) = build_matcher(needle, options) else {
		return -1;
	};
	if options.contains(SearchOptions::FORWARD) {
		if let Some(m) = re.find(&haystack[start_byte..]) {
			let byte_pos = start_byte + m.start();
			let display_pos = byte_to_display_index(haystack, byte_pos);
			return i64::try_from(display_pos).unwrap_or(-1);
		}
	} else {
		let mut last: Option<usize> = None;
		let end_byte = start_byte.min(haystack.len());
		for m in re.find_iter(&haystack[..end_byte]) {
			last = Some(m.start());
		}
		if let Some(pos) = last {
			let display_pos = byte_to_display_index(haystack, pos);
			return i64::try_from(display_pos).unwrap_or(-1);
		}
	}
	-1
}

/// Every match of `needle` in `haystack`, as display-unit `(start, end)` spans in document order.
/// Direction and wrap have no meaning here. Zero-length matches (e.g. an empty `x*` regex match)
/// are skipped; an empty needle or an invalid regex yields no spans.
#[must_use]
pub fn reader_search_all(haystack: &str, needle: &str, options: SearchOptions) -> Vec<(i64, i64)> {
	if needle.is_empty() {
		return Vec::new();
	}
	let Some(re) = build_matcher(needle, options) else {
		return Vec::new();
	};
	// Collect raw byte ranges first (matches come back ordered and non-overlapping), then convert
	// every boundary to display units in one forward pass - converting each one separately would rescan
	// the whole text per match and be quadratic for documents with many occurrences.
	let mut byte_ranges: Vec<(usize, usize)> = Vec::new();
	for m in re.find_iter(haystack) {
		if m.start() != m.end() {
			byte_ranges.push((m.start(), m.end()));
		}
	}
	if byte_ranges.is_empty() {
		return Vec::new();
	}
	let mut targets: Vec<usize> = Vec::with_capacity(byte_ranges.len() * 2);
	for (start, end) in &byte_ranges {
		targets.push(*start);
		targets.push(*end);
	}
	let display = display_offsets_at(haystack, &targets);
	byte_ranges
		.into_iter()
		.enumerate()
		.map(|(index, _)| {
			let start = i64::try_from(display[index * 2]).unwrap_or(-1);
			let end = i64::try_from(display[index * 2 + 1]).unwrap_or(-1);
			(start, end)
		})
		.collect()
}

#[must_use]
pub fn reader_search_with_wrap(haystack: &str, needle: &str, start: i64, options: SearchOptions) -> ffi::SearchResult {
	let position = reader_search(haystack, needle, start, options);
	if position >= 0 {
		return ffi::SearchResult { found: true, wrapped: false, position };
	}
	let wrap_pos =
		if options.contains(SearchOptions::FORWARD) { 0 } else { i64::try_from(display_len(haystack)).unwrap_or(0) };
	let wrapped_position = reader_search(haystack, needle, wrap_pos, options);
	if wrapped_position >= 0 {
		return ffi::SearchResult { found: true, wrapped: true, position: wrapped_position };
	}
	ffi::SearchResult { found: false, wrapped: false, position: -1 }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reader_search_handles_basic_and_whole_word() {
		let haystack = "Hello world";
		let options = SearchOptions::FORWARD;
		assert_eq!(reader_search(haystack, "hello", 0, options), 0);
		let haystack = "hello_world";
		let options = SearchOptions::FORWARD | SearchOptions::WHOLE_WORD;
		assert_eq!(reader_search(haystack, "hello", 0, options), -1);
	}

	#[test]
	fn reader_search_returns_display_offsets() {
		let haystack = "a😀b";
		let options = SearchOptions::FORWARD;
		let expected = i64::try_from(display_len("a😀")).unwrap();
		assert_eq!(reader_search(haystack, "b", 0, options), expected);
	}

	#[test]
	fn reader_search_starts_from_a_display_offset() {
		let haystack = "😀b😀b";
		let options = SearchOptions::FORWARD;
		let start = i64::try_from(display_len("😀b")).unwrap();
		assert_eq!(reader_search(haystack, "b", start, options), i64::try_from(display_len("😀b😀")).unwrap());
	}

	#[test]
	fn reader_search_with_wrap_backward_starts_from_the_display_end() {
		let haystack = "😀😀a";
		let result = reader_search_with_wrap(haystack, "a", 0, SearchOptions::empty());
		assert!(result.found);
		assert!(result.wrapped);
		assert_eq!(result.position, i64::try_from(display_len("😀😀")).unwrap());
	}

	#[test]
	fn reader_search_all_returns_display_spans() {
		let haystack = "😀ab 😀ab";
		let spans = reader_search_all(haystack, "ab", SearchOptions::empty());
		let first = i64::try_from(display_len("😀")).unwrap();
		let second = i64::try_from(display_len("😀ab 😀")).unwrap();
		assert_eq!(spans, vec![(first, first + 2), (second, second + 2)]);
	}

	#[cfg(not(any(windows, target_os = "macos")))]
	#[test]
	fn reader_search_counts_scalars_off_windows_and_macos() {
		assert_eq!(reader_search("a😀b", "b", 0, SearchOptions::FORWARD), 2);
	}

	#[cfg(any(windows, target_os = "macos"))]
	#[test]
	fn reader_search_counts_utf16_units_on_windows_and_macos() {
		assert_eq!(reader_search("a😀b", "b", 0, SearchOptions::FORWARD), 3);
	}

	#[test]
	fn reader_search_handles_match_case() {
		let haystack = "Hello hello";
		let options = SearchOptions::FORWARD | SearchOptions::MATCH_CASE;
		assert_eq!(reader_search(haystack, "hello", 0, options), 6);
		assert_eq!(reader_search(haystack, "Hello", 0, options), 0);
		let options = SearchOptions::FORWARD;
		assert_eq!(reader_search(haystack, "HELLO", 0, options), 0);
	}

	#[test]
	fn reader_search_with_wrap_wraps_forward() {
		let haystack = "abc";
		let options = SearchOptions::FORWARD;
		let result = reader_search_with_wrap(haystack, "a", 1, options);
		assert!(result.found);
		assert!(result.wrapped);
		assert_eq!(result.position, 0);
	}

	#[test]
	fn reader_search_backward_finds_previous_match() {
		let haystack = "one two one";
		let options = SearchOptions::empty();
		assert_eq!(reader_search(haystack, "one", 11, options), 8);
	}

	#[test]
	fn reader_search_with_regex_invalid_pattern_returns_not_found() {
		let haystack = "abc";
		let options = SearchOptions::FORWARD | SearchOptions::REGEX;
		assert_eq!(reader_search(haystack, "(", 0, options), -1);
	}

	#[test]
	fn reader_search_whole_word_positive_case() {
		let haystack = "alpha beta gamma";
		let options = SearchOptions::FORWARD | SearchOptions::WHOLE_WORD;
		assert_eq!(reader_search(haystack, "beta", 0, options), 6);
	}

	#[test]
	fn reader_search_clamps_negative_start_to_zero() {
		let haystack = "abc";
		let options = SearchOptions::FORWARD;
		assert_eq!(reader_search(haystack, "a", -500, options), 0);
	}

	#[test]
	fn reader_search_with_wrap_wraps_backward() {
		let haystack = "abca";
		let options = SearchOptions::empty();
		let result = reader_search_with_wrap(haystack, "a", 0, options);
		assert!(result.found);
		assert!(result.wrapped);
		assert_eq!(result.position, 3);
	}

	#[test]
	fn reader_search_all_returns_every_plain_match_in_order() {
		let haystack = "blah on page one, blah again, and blah";
		let options = SearchOptions::empty();
		let spans = reader_search_all(haystack, "blah", options);
		assert_eq!(spans.len(), 3);
		assert_eq!(spans[0].0, 0);
		// "blah" is four ASCII chars, so every span is four display units long.
		assert!(spans.iter().all(|(start, end)| end - start == 4));
		assert!(spans.windows(2).all(|w| w[0].0 < w[1].0));
	}

	#[test]
	fn reader_search_all_respects_match_case() {
		let haystack = "Hello hello HELLO";
		let case_sensitive = SearchOptions::MATCH_CASE;
		assert_eq!(reader_search_all(haystack, "hello", case_sensitive).len(), 1);
		let insensitive = SearchOptions::empty();
		assert_eq!(reader_search_all(haystack, "hello", insensitive).len(), 3);
	}

	#[test]
	fn reader_search_all_respects_whole_word() {
		let haystack = "the cat and the theater";
		let options = SearchOptions::WHOLE_WORD;
		assert_eq!(reader_search_all(haystack, "the", options).len(), 2);
		let options = SearchOptions::empty();
		assert_eq!(reader_search_all(haystack, "the", options).len(), 3);
	}

	#[test]
	fn reader_search_all_regex_spans_the_actual_match() {
		let haystack = "12 and 345 and 6";
		let options = SearchOptions::REGEX;
		let spans = reader_search_all(haystack, r"\d+", options);
		assert_eq!(spans.len(), 3);
		// Each span covers the whole run of digits, not just one.
		assert_eq!((spans[0].1 - spans[0].0), 2);
		assert_eq!((spans[1].1 - spans[1].0), 3);
		assert_eq!((spans[2].1 - spans[2].0), 1);
	}

	#[test]
	fn reader_search_all_skips_zero_length_matches() {
		// `a*` also matches the empty gaps between characters; only the real run should survive.
		let haystack = "aaab";
		let options = SearchOptions::REGEX;
		let spans = reader_search_all(haystack, "a*", options);
		assert_eq!(spans.len(), 1);
		assert_eq!((spans[0].0, spans[0].1), (0, 3));
	}

	#[test]
	fn reader_search_all_invalid_regex_yields_no_spans() {
		let haystack = "abc";
		let options = SearchOptions::REGEX;
		assert!(reader_search_all(haystack, "(", options).is_empty());
		assert!(reader_search_all(haystack, "", options).is_empty());
	}
}
