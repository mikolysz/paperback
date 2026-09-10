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
	let input = format!("<html><body>{body}</body></html>");
	if xml {
		let mut converter = XmlToText::new();
		assert!(converter.convert(&input));
		Converted {
			text: converter.get_text(),
			maths: converter.get_maths().to_vec(),
			headings: converter.get_headings().to_vec(),
			links: converter.get_links().to_vec(),
			ids: converter.get_id_positions().clone(),
		}
	} else {
		let mut converter = HtmlToText::new();
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
