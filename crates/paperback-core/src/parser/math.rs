//! MathML-to-speech conversion via MathCAT (the library NVDA and JAWS use).

// MathCAT keeps its state in thread-locals (it is designed for
// one-expression-at-a-time assistive-technology use), so each thread that
// speaks math initializes its own instance lazily. Books without math never
// load the speech rules, which take ~100 ms to initialize.
//
// There's a memo cache,  shared across threads, because math books repeat expressions heavily, including across sections. The
// rules ship embedded in the binary (the crate's `include-zip` feature
// builds a virtual filesystem with a top-level `Rules` directory), so no
// additional files need to be shipped with Paperback.

use std::{
	cell::OnceCell,
	collections::HashMap,
	sync::{LazyLock, Mutex},
};

use crate::util::text::{collapse_whitespace, trim_string};

/// Whitespace-normalized MathML fragment -> spoken text (None = MathCAT failed on it).
// The Mutex lets us share the cache across threads, the LazyLock layer exists only because a static's
// initializer must be const and `HashMap::new()` isn't (RandomState seeds itself from the OS at runtime).
static CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
	static MATHCAT_INIT: OnceCell<()> = const { OnceCell::new() };
}

fn init() {
	// The rules are compiled into the binary, so unlike a rules directory on disk they cannot be missing or corrupt.
	// A failure here would mean a defective mathcat build, which this module's tests catch.
	libmathcat::set_rules_dir("Rules".to_string()).expect("embedded MathCAT rules failed to load");
}

/// Spoken rendering of a math element: MathCAT speech when it accepts the
/// markup, else the alttext attribute (MathML3§2.2.1), else the element's raw
/// text content. None when every source is empty, in which case the element
/// produces no output at all.
pub fn spoken_math_text(mathml: &str, alttext: Option<&str>, text_content: impl FnOnce() -> String) -> Option<String> {
	speak_mathml(mathml)
		.or_else(|| alttext.map(|alt| trim_string(&collapse_whitespace(alt))).filter(|alt| !alt.is_empty()))
		.or_else(|| Some(trim_string(&collapse_whitespace(&text_content()))).filter(|text| !text.is_empty()))
}

/// Spoken text for a MathML fragment, or None when MathCAT rejects the
/// markup as malformed.
fn speak_mathml(mathml: &str) -> Option<String> {
	MATHCAT_INIT.with(|cell| {
		cell.get_or_init(init);
	});
	// Whitespace between MathML tags is insignificant, so normalizing the key
	// lets expressions that differ only in source indentation share one entry.
	let key = collapse_whitespace(mathml);
	if let Some(hit) = CACHE.lock().unwrap().get(&key) {
		return hit.clone();
	}
	// Converted outside the lock: large expressions take milliseconds, and
	// holding the lock would serialize workers. In the unlikely
	// event where two threads compute the same expression,  they just do it twice.
	let spoken = libmathcat::set_mathml(mathml.to_string())
		.and_then(|_| libmathcat::get_spoken_text())
		.ok()
		.map(|text| trim_string(&collapse_whitespace(&text)))
		.filter(|text| !text.is_empty());
	CACHE.lock().unwrap().insert(key, spoken.clone());
	spoken
}

/// If the math element uses namespaced tags (xmlns:foo declared as the MathML
/// namespace on the root element ), strip the foo: prefix from every tag.
/// This is similar in spirit to the hack MathCat itself uses for the same purpose, although ours
/// doesn't strip other, unrelated namespaces.
/// This matters because web browsers don't understand xml namespaces; verified with Firefox,
// Chrome and Safari.
pub fn normalize_fragment(raw: String) -> String {
	let Some(prefix) = raw[1..].split(':').next().filter(|p| !p.contains('>') && !p.contains(' ')) else {
		return raw;
	};
	raw.replace(&format!("<{prefix}:"), "<").replace(&format!("</{prefix}:"), "</")
}

#[cfg(test)]
mod tests {
	use rstest::rstest;

	use super::*;

	#[rstest]
	fn speaks_simple_expression() {
		let spoken =
			speak_mathml("<math><msqrt><mi>x</mi></msqrt></math>").expect("MathCAT should speak a trivial expression");
		assert!(spoken.contains("square root"), "spoken text was: {spoken}");
	}

	#[rstest]
	fn repeated_expressions_stay_available() {
		let mathml = "<math><mfrac><mi>a</mi><mi>b</mi></mfrac></math>";
		let first = speak_mathml(mathml);
		let second = speak_mathml(mathml);
		assert!(first.is_some());
		assert_eq!(first, second);
	}

	#[rstest]
	fn garbage_input_does_not_panic() {
		// Any result is fine as long as it returns; a later valid call must still work.
		let _ = speak_mathml("<math><nonsense");
		assert!(speak_mathml("<math><mn>2</mn></math>").is_some());
	}

	#[rstest]
	#[case("<m:math><m:mi>x</m:mi></m:math>", "<math><mi>x</mi></math>")]
	#[case("<math:math><math:mi>x</math:mi></math:math>", "<math><mi>x</mi></math>")]
	#[case("<math><mi>x</mi></math>", "<math><mi>x</mi></math>")]
	#[case(
		"<math xmlns=\"http://www.w3.org/1998/Math/MathML\"><mi>x</mi></math>",
		"<math xmlns=\"http://www.w3.org/1998/Math/MathML\"><mi>x</mi></math>"
	)]
	fn normalize_fragment_strips_prefixes(#[case] input: &str, #[case] expected: &str) {
		assert_eq!(normalize_fragment(input.to_string()), expected);
	}
}
