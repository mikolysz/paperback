//! Opt-in end-to-end check against the freely available IDPF `MathML` EPUB.
//! See doc/math-testing.md for the pinned download and invocation.

use std::env;

use paperback_core::{
	document::{DocumentHandle, MarkerType, ParserContext},
	parser::parse_document,
	reader_core::reader_navigate,
	types::{NavDirection, NavRequest, NavTarget},
	util::text::display_len,
};

#[test]
#[ignore = "requires PAPERBACK_MATH_EPUB pointing to IDPF's linear-algebra.epub"]
fn linear_algebra_mathml_epub() {
	let path = env::var("PAPERBACK_MATH_EPUB").expect("set PAPERBACK_MATH_EPUB; see doc/math-testing.md");
	let document = parse_document(&ParserContext::new(path).with_render_tables_inline(true)).unwrap();
	assert_eq!(document.title, "A First Course in Linear Algebra");
	assert!(document.buffer.content.contains("x^2+y^2"));
	assert!(document.buffer.content.contains("(sqrt(3))/2"));
	assert!(!document.buffer.content.contains("Alternative text not available"));
	let handle = DocumentHandle::new(document);
	let buffer = &handle.document().buffer;
	let maths: Vec<_> = buffer.markers.iter().filter(|marker| marker.mtype == MarkerType::Math).collect();
	// Includes the 505 expressions contained in HTML table cells.
	assert_eq!(maths.len(), 10_281);
	let mut previous = -1;
	for math in maths {
		assert!(!math.text.is_empty());
		assert_eq!(math.length, display_len(&math.text));
		assert!(math.position + math.length <= buffer.char_count());
		let start = buffer.byte_index_for_char(math.position);
		let end = buffer.byte_index_for_char(math.position + math.length);
		assert_eq!(&buffer.content[start..end], math.text, "formula at {}", math.position);
		let fragment = roxmltree::Document::parse(&math.reference).expect("standalone MathML");
		assert_eq!(fragment.root_element().tag_name().name(), "math");
		let result = reader_navigate(
			&handle,
			&NavRequest {
				position: previous,
				wrap: false,
				direction: NavDirection::Next,
				target: NavTarget::Math,
				level_filter: 0,
			},
		);
		assert!(result.found);
		assert_eq!(result.offset, math.position);
		previous = i64::try_from(result.offset).unwrap();
	}
}
