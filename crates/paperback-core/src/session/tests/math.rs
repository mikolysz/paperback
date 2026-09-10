use super::*;

const MATHML: &str = "<math><mi>x</mi><mo>=</mo><mn>1</mn></math>";

fn math_session() -> DocumentSession {
	let mut buffer = DocumentBuffer::with_content("before\nx = 1 after".to_string());
	buffer.add_marker(
		Marker::new(MarkerType::Math, 7)
			.with_length(5)
			.with_text("x = 1".to_string())
			.with_reference(MATHML.to_string()),
	);
	session_from_buffer(buffer)
}

#[test]
fn reference_uses_half_open_display_extent() {
	let session = math_session();
	assert_eq!(session.get_math_at_position(7).as_deref(), Some(MATHML));
	assert_eq!(session.get_math_at_position(11).as_deref(), Some(MATHML));
	assert!(session.get_math_at_position(12).is_none());
	assert!(session.get_math_at_position(2).is_none());
}

#[test]
fn missing_reference_does_not_activate() {
	let mut buffer = DocumentBuffer::with_content("x".to_string());
	buffer.add_marker(Marker::new(MarkerType::Math, 0).with_length(1));
	assert!(session_from_buffer(buffer).get_math_at_position(0).is_none());
}

#[test]
fn navigation_finds_math_and_wraps() {
	let session = math_session();
	let result = session.navigate_math(0, false, true);
	assert!(result.found);
	assert_eq!(result.offset, 7);
	assert_eq!(result.marker_text, "x = 1");
	assert!(!session.navigate_math(0, false, false).found);
	assert_eq!(session.navigate_math(12, false, false).offset, 7);
	let wrapped = session.navigate_math(12, true, true);
	assert!(wrapped.found && wrapped.wrapped);
	assert_eq!(wrapped.offset, 7);
}

#[test]
fn supported_only_when_math_is_present() {
	assert!(math_session().get_supported_segment_types_ffi().iter().any(|t| matches!(t, SegmentTypeFfi::Math)));
	let without_math = sample_session(ParserFlags::NONE);
	assert!(without_math.navigate_math(0, false, true).not_supported);
	assert!(!without_math.get_supported_segment_types_ffi().iter().any(|t| matches!(t, SegmentTypeFfi::Math)));
}

#[test]
fn text_segment_navigates_math() {
	let segment = math_session().get_text_segment(0, SegmentTypeFfi::Math, SegmentDirectionFfi::Next);
	assert!(segment.found);
	assert_eq!(segment.start_pos, 7);
	assert_eq!(segment.end_pos, 12);
	assert_eq!(segment.text, "x = 1");
}
