//! MathML conversion shared by the XML/HTML text engines. MathCAT's rules are embedded;
//! its per-thread state is initialized lazily, including on EPUB's rayon workers.

use std::{
	cell::OnceCell,
	collections::HashMap,
	sync::{LazyLock, Mutex},
};

use roxmltree::Node;
use scraper::ElementRef;

use crate::util::{
	html::{escape, escape_attr},
	text::collapse_whitespace,
};

const MATHML_NAMESPACE: &str = "http://www.w3.org/1998/Math/MathML";
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct MathCache {
	entries: HashMap<String, Option<String>>,
	bytes: usize,
}

// Share repeated expressions across sections without retaining an unbounded history of books.
static CACHE: LazyLock<Mutex<MathCache>> = LazyLock::new(|| Mutex::new(MathCache::default()));

thread_local! {
	static MATHCAT_INIT: OnceCell<bool> = const { OnceCell::new() };
}

fn init() -> bool {
	// TODO: make the output code configurable when math preferences are implemented.
	// ASCIIMath is a text notation, despite being exposed through MathCAT's braille API.
	let result = libmathcat::set_rules_dir("Rules".to_string())
		.and_then(|()| libmathcat::set_preference("Language".to_string(), "en".to_string()))
		.and_then(|()| libmathcat::set_preference("BrailleCode".to_string(), "ASCIIMath".to_string()));
	if let Err(error) = result {
		tracing::warn!(%error, "failed to initialize embedded MathCAT rules");
		return false;
	}
	true
}

/// AsciiMath when conversion succeeds, then the author's alternative text, then raw text.
/// An empty expression with no fallback emits neither text nor a marker.
pub(super) fn math_text(mathml: &str, alttext: Option<&str>, text_content: impl FnOnce() -> String) -> Option<String> {
	asciimath(mathml).or_else(|| alttext.and_then(normalized_text)).or_else(|| normalized_text(&text_content()))
}

fn normalized_text(text: &str) -> Option<String> {
	let text = collapse_whitespace(text).trim().to_string();
	(!text.is_empty()).then_some(text)
}

fn asciimath(mathml: &str) -> Option<String> {
	if !MATHCAT_INIT.with(|cell| *cell.get_or_init(init)) {
		return None;
	}
	// Do not collapse whitespace in the key: whitespace inside mtext and attribute values can
	// be significant. Serialization already canonicalizes XML element/attribute quoting.
	if let Some(hit) = CACHE.lock().unwrap().entries.get(mathml) {
		return hit.clone();
	}
	// Convert outside the lock so independent EPUB sections remain parallel.
	let result = libmathcat::set_mathml(mathml.to_string()).and_then(|_| libmathcat::get_braille(String::new()));
	let rendered = match result {
		Ok(text) => normalized_text(&text),
		Err(error) => {
			tracing::debug!(%error, "MathCAT rejected an expression; using alternative text");
			None
		}
	};
	let bytes = mathml.len() + rendered.as_ref().map_or(0, String::len);
	if bytes <= MAX_CACHE_BYTES {
		let mut cache = CACHE.lock().unwrap();
		if !cache.entries.contains_key(mathml) {
			if cache.bytes + bytes > MAX_CACHE_BYTES {
				cache.entries.clear();
				cache.bytes = 0;
			}
			cache.bytes += bytes;
			cache.entries.insert(mathml.to_string(), rendered.clone());
		}
	}
	rendered
}

/// Serialize an isolated XML MathML subtree. Working from the parsed tree expands entities
/// and resolves inherited/mixed MathML prefixes; string replacement cannot do either safely.
pub(super) fn xml_fragment(node: Node<'_, '_>) -> String {
	let mut output = String::new();
	serialize_xml(node, node.tag_name().namespace(), "", &mut output);
	output
}

fn serialize_xml(node: Node<'_, '_>, math_namespace: Option<&str>, inherited_namespace: &str, output: &mut String) {
	if node.is_text() {
		output.push_str(&escape(node.text().unwrap_or("")));
		return;
	}
	if !node.is_element() {
		return;
	}
	let name = node.tag_name().name();
	let namespace = if node.tag_name().namespace() == math_namespace {
		MATHML_NAMESPACE
	} else {
		node.tag_name().namespace().unwrap_or("")
	};
	output.push('<');
	output.push_str(name);
	if namespace != inherited_namespace {
		output.push_str(" xmlns=\"");
		output.push_str(&escape_attr(namespace));
		output.push('"');
	}
	let mut declared_prefixes = Vec::new();
	for attr in node.attributes() {
		let prefix = attr.namespace().map(|ns| {
			if ns == "http://www.w3.org/XML/1998/namespace" {
				return "xml";
			}
			let prefix = node.lookup_prefix(ns).unwrap_or("attr");
			if !declared_prefixes.contains(&prefix) {
				output.push_str(" xmlns:");
				output.push_str(prefix);
				output.push_str("=\"");
				output.push_str(&escape_attr(ns));
				output.push('"');
				declared_prefixes.push(prefix);
			}
			prefix
		});
		output.push(' ');
		if let Some(prefix) = prefix {
			output.push_str(prefix);
			output.push(':');
		}
		output.push_str(attr.name());
		output.push_str("=\"");
		output.push_str(&escape_attr(attr.value()).replace('<', "&lt;"));
		output.push('"');
	}
	output.push('>');
	for child in node.children() {
		serialize_xml(child, math_namespace, namespace, output);
	}
	output.push_str("</");
	output.push_str(name);
	output.push('>');
}

/// Text extraction for headings, list labels and links must use the same formula rendering
/// as the reading buffer, rather than concatenating MathML's token/annotation text.
pub(super) fn collect_xml_text(node: Node<'_, '_>) -> String {
	fn collect(node: Node<'_, '_>, output: &mut String) {
		if node.is_element() && node.tag_name().name() == "math" {
			if let Some(text) = math_text(&xml_fragment(node), node.attribute("alttext"), || {
				crate::parser::util::xml::collect_element_text(node)
			}) {
				output.push_str(&text);
			}
		} else if node.is_text() {
			output.push_str(node.text().unwrap_or(""));
		} else {
			for child in node.children() {
				collect(child, output);
			}
		}
	}
	let mut output = String::new();
	collect(node, &mut output);
	output.trim().to_string()
}

pub(super) fn dom_math_text(element: ElementRef<'_>) -> Option<String> {
	math_text(&element.html(), element.attr("alttext"), || element.text().collect())
}

#[cfg(test)]
mod tests;
