# MathML regression testing

MathCAT's embedded rules currently render formulas as AsciiMath through its
`BrailleCode = ASCIIMath` output. Output preferences remain unimplemented.
The original MathML is retained for Formula View. Both the XML and HTML converters
record display-unit spans, including visible formulas inside inline tables and
the first row of table placeholders.

Run the focused tests:

```sh
cargo test -p paperback-core math
```

For the full-book check, download the freely available IDPF EPUB 3 sample,
*A First Course in Linear Algebra* by Robert A. Beezer (GNU FDL 1.2).
The [sample catalog](https://idpf.github.io/epub3-samples/30/samples.html)
describes the book; the commands below pin its 2023-07-04 release:

```sh
curl -fL https://github.com/IDPF/epub3-samples/releases/download/20230704/linear-algebra.epub -o /tmp/linear-algebra.epub
cargo run -p pb -- /tmp/linear-algebra.epub --no-prompt -o /tmp/linear-algebra.txt
cargo run -p pb -- /tmp/linear-algebra.epub --no-prompt --metadata
PAPERBACK_MATH_EPUB=/tmp/linear-algebra.epub cargo test -p paperback-core --test math_epub -- --ignored
```

The integration test checks all 10,281 formula markers (including 505 in HTML
tables), their exact text spans, retained MathML, and forward navigation.
It is opt-in so normal tests need neither a network connection nor the book.
Typical text includes `x^2+y^2` and `(sqrt(3))/2`; formulas should not fall back
to the sample's generic “Alternative text not available”.

On desktop, check `M`/`Shift+M`, rebinding these actions in Customize Keyboard
Shortcuts, and opening Formula View with `Enter`/`Space` both near the beginning
and far into the book (after the reader's text window has moved).
