use std::{collections::HashMap, thread};

use rstest::rstest;

use super::*;
use crate::{
	parser::convert::{
		html_to_text::{HtmlSourceMode, HtmlToText},
		xml_to_text::XmlToText,
	},
	types::{HeadingInfo, LinkInfo, MathInfo},
	util::text::display_len,
};

#[rstest]
#[case("<mi>x</mi><mo>=</mo><mn>1</mn>", "x = 1")]
#[case("<mfrac><mi>a</mi><mi>b</mi></mfrac>", "a/b")]
#[case("<msqrt><mi>x</mi></msqrt>", "sqrt(x)")]
#[case("<msup><mi>x</mi><mn>2</mn></msup>", "x^2")]
fn renders_asciimath(#[case] body: &str, #[case] expected: &str) {
	assert_eq!(asciimath(&format!("<math>{body}</math>")).as_deref(), Some(expected));
}

#[test]
fn conversion_failure_uses_alttext_then_text_and_recovers() {
	assert_eq!(
		math_text("<math><broken", Some("  alternative\n text "), || "raw".to_string()).as_deref(),
		Some("alternative text")
	);
	assert_eq!(math_text("<math><broken", Some(" "), || " raw\n text ".to_string()).as_deref(), Some("raw text"));
	assert!(math_text("<math><broken", None, String::new).is_none());
	assert_eq!(asciimath("<math><mi>y</mi></math>").as_deref(), Some("y"));
}

#[test]
fn initializes_on_each_worker_and_reuses_expressions() {
	let workers: Vec<_> = (0..4)
		.map(|_| {
			thread::spawn(|| {
				for _ in 0..3 {
					assert_eq!(asciimath("<math><msqrt><mn>2</mn></msqrt></math>").as_deref(), Some("sqrt(2)"));
				}
			})
		})
		.collect();
	for worker in workers {
		worker.join().unwrap();
	}
}

#[test]
fn xml_fragment_resolves_inherited_prefixes_entities_and_foreign_namespaces() {
	let xml = r#"<body xmlns:m="http://www.w3.org/1998/Math/MathML" xmlns:q="http://www.w3.org/1998/Math/MathML" xmlns:extra="urn:extra"><m:math alttext="x &lt; &quot;y&quot;" extra:label="a &amp; b"><q:mi>x</q:mi><m:mo>&lt;</m:mo><mi xmlns="http://www.w3.org/1998/Math/MathML">y</mi><extra:note>foreign</extra:note></m:math></body>"#;
	let document = roxmltree::Document::parse(xml).unwrap();
	let fragment = xml_fragment(document.root_element().first_element_child().unwrap());
	let parsed = roxmltree::Document::parse(&fragment).unwrap();
	let math = parsed.root_element();
	assert_eq!(math.tag_name().namespace(), Some(MATHML_NAMESPACE));
	assert_eq!(math.attribute("alttext"), Some("x < \"y\""));
	assert_eq!(math.attribute(("urn:extra", "label")), Some("a & b"));
	let elements: Vec<_> = math.children().filter(Node::is_element).collect();
	assert!(elements[..3].iter().all(|n| n.tag_name().namespace() == Some(MATHML_NAMESPACE)));
	assert_eq!(elements[1].text(), Some("<"));
	assert_eq!(elements[3].tag_name().namespace(), Some("urn:extra"));
}

struct Converted {
	text: String,
	maths: Vec<MathInfo>,
	headings: Vec<HeadingInfo>,
	links: Vec<LinkInfo>,
	ids: HashMap<String, usize>,
}

fn convert(body: &str, xml: bool) -> Converted {
	convert_with_tables(body, xml, false)
}

fn convert_with_tables(body: &str, xml: bool, inline: bool) -> Converted {
	let input = format!("<html><body>{body}</body></html>");
	if xml {
		let mut converter = XmlToText::with_render_tables_inline(inline);
		assert!(converter.convert(&input));
		Converted {
			text: converter.get_text(),
			maths: converter.get_maths().to_vec(),
			headings: converter.get_headings().to_vec(),
			links: converter.get_links().to_vec(),
			ids: converter.get_id_positions().clone(),
		}
	} else {
		let mut converter = HtmlToText::with_render_tables_inline(inline);
		assert!(converter.convert(&input, HtmlSourceMode::NativeHtml));
		Converted {
			text: converter.get_text(),
			maths: converter.get_maths().to_vec(),
			headings: converter.get_headings().to_vec(),
			links: converter.get_links().to_vec(),
			ids: converter.get_id_positions().clone(),
		}
	}
}

#[rstest]
#[case(false)]
#[case(true)]
fn inline_and_block_math_preserve_text_extents_and_anchors(#[case] xml: bool) {
	let converted = convert(
		r#"<p>😀 Let <math id="inline"><msup><mi id="token">x</mi><mn>2</mn></msup></math> be positive.</p><math display="block" id="block"><mfrac><mi>a</mi><mi>b</mi></mfrac></math><h2>After</h2>"#,
		xml,
	);
	assert_eq!(converted.text, "😀 Let x^2 be positive.\na/b\nAfter");
	assert_eq!(converted.maths.len(), 2);
	for (math, prefix, expected) in
		[(&converted.maths[0], "😀 Let ", "x^2"), (&converted.maths[1], "😀 Let x^2 be positive.\n", "a/b")]
	{
		assert_eq!(math.offset, display_len(prefix));
		assert_eq!(math.length, display_len(expected));
		assert_eq!(math.text, expected);
		assert!(math.mathml.contains("<math"));
		assert!(math.mathml.contains("</math>"));
	}
	assert_eq!(converted.ids["inline"], converted.maths[0].offset);
	assert_eq!(converted.ids["token"], converted.maths[0].offset);
	assert_eq!(converted.ids["block"], converted.maths[1].offset);
	assert_eq!(converted.headings[0].offset, display_len("😀 Let x^2 be positive.\na/b\n"));
}

#[rstest]
#[case(false)]
#[case(true)]
fn math_in_links_and_heading_labels_is_not_flattened(#[case] xml: bool) {
	let converted = convert(
		r##"<h2>Square <math><msup><mi>x</mi><mn>2</mn></msup></math></h2><p>See <a href="#target">the <math><msqrt><mi>x</mi></msqrt></math> formula</a>.</p>"##,
		xml,
	);
	assert_eq!(converted.text, "Square x^2\nSee the sqrt(x) formula.");
	assert_eq!(converted.headings[0].text, "Square x^2");
	assert_eq!(converted.links[0].text, "the sqrt(x) formula");
	assert_eq!(converted.maths.len(), 2);
	assert_eq!(converted.maths[1].offset, display_len("Square x^2\nSee the "));
	assert_eq!(converted.maths[1].length, display_len("sqrt(x)"));
}

#[rstest]
#[case(false)]
#[case(true)]
fn final_block_math_does_not_extend_beyond_buffer(#[case] xml: bool) {
	let converted = convert(r#"<p>Before</p><math display="block"><mi>x</mi></math>"#, xml);
	let math = &converted.maths[0];
	assert_eq!(converted.text, "Before\nx");
	assert_eq!(math.offset + math.length, display_len(&converted.text));
}

#[rstest]
#[case(false)]
#[case(true)]
fn table_text_uses_asciimath(#[case] xml: bool) {
	let converted = convert("<table><tr><td><math><msup><mi>x</mi><mn>2</mn></msup></math></td></tr></table>", xml);
	assert!(converted.text.contains("x^2"), "{}", converted.text);
}

#[test]
fn prefixed_math_in_xml_is_converted() {
	let converted = convert(
		r#"<p xmlns:m="http://www.w3.org/1998/Math/MathML">Let <m:math><m:msqrt><m:mi>x</m:mi></m:msqrt></m:math>.</p>"#,
		true,
	);
	assert_eq!(converted.text, "Let sqrt(x).");
	assert_eq!(converted.maths.len(), 1);
	assert!(converted.maths[0].mathml.contains(MATHML_NAMESPACE));
}

#[rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
fn table_math_spans_follow_cells_rows_and_placeholder_visibility(#[case] xml: bool, #[case] inline: bool) {
	let converted = convert_with_tables(
		"<p>Start</p><table><tr><td>  a <math><msup><mi>x</mi><mn>2</mn></msup></math></td><td> <math><msqrt><mi>y</mi></msqrt></math></td></tr><tr><td><math><mfrac><mi>a</mi><mi>b</mi></mfrac></math></td></tr></table><p>After</p>",
		xml,
		inline,
	);
	assert_eq!(
		converted.text,
		if inline { "Start\na x^2\tsqrt(y)\na/b\nAfter" } else { "Start\n[Table]: a x^2 sqrt(y)\nAfter" }
	);
	assert_eq!(converted.maths.len(), if inline { 3 } else { 2 });
	let buffer = crate::document::DocumentBuffer::with_content(converted.text);
	for math in converted.maths {
		let start = buffer.byte_index_for_char(math.offset);
		let end = buffer.byte_index_for_char(math.offset + math.length);
		assert_eq!(&buffer.content[start..end], math.text);
		assert!(math.mathml.contains("<math"));
	}
}

#[test]
fn prefixed_math_in_xml_table_keeps_inherited_namespaces() {
	let converted = convert_with_tables(
		r#"<div xmlns:m="http://www.w3.org/1998/Math/MathML"><table><tr><td><m:math><m:msqrt><m:mi>x</m:mi></m:msqrt></m:math></td></tr></table></div>"#,
		true,
		true,
	);
	assert_eq!(converted.text, "sqrt(x)");
	assert_eq!(converted.maths.len(), 1);
}

#[test]
fn html_math_fragment_does_not_leave_html_only_entities() {
	let converted = convert("<math alttext='a&nbsp;b'><mtext>a&nbsp;b</mtext></math>", false);
	let fragment = &converted.maths[0].mathml;
	assert!(!fragment.contains("&nbsp;"));
	let document = roxmltree::Document::parse(fragment).unwrap();
	assert_eq!(document.root_element().attribute("alttext"), Some("a\u{00a0}b"));
}

#[rstest]
#[case("")]
#[case("Before ")]
fn formula_in_link_counts_boundary_whitespace_once(#[case] prefix: &str) {
	let converted = convert(&format!(r##"<p>{prefix}<a href="#x">  <math><mi>x</mi></math></a></p>"##), false);
	assert_eq!(converted.text, format!("{prefix}x"));
	assert_eq!(converted.maths[0].offset, display_len(prefix));
}

#[rstest]
#[case(false)]
#[case(true)]
fn reusing_a_converter_clears_math_markers(#[case] xml: bool) {
	let with_math = "<html><body><math><mi>x</mi></math></body></html>";
	let plain = "<html><body>Plain</body></html>";
	if xml {
		let mut converter = XmlToText::new();
		assert!(converter.convert(with_math));
		assert_eq!(converter.get_maths().len(), 1);
		assert!(converter.convert(plain));
		assert!(converter.get_maths().is_empty());
	} else {
		let mut converter = HtmlToText::new();
		assert!(converter.convert(with_math, HtmlSourceMode::NativeHtml));
		assert_eq!(converter.get_maths().len(), 1);
		assert!(converter.convert(plain, HtmlSourceMode::NativeHtml));
		assert!(converter.get_maths().is_empty());
	}
}
