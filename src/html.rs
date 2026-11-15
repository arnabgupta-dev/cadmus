//! Rich-text fields: cleaning the HTML that is stored, and plain text for lists.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ammonia::Builder;

/// The tags the editor makes, and nothing else.
const TAGS: &[&str] = &[
    "p",
    "br",
    "h1",
    "h2",
    "h3",
    "h4",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "sub",
    "sup",
    "a",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "img",
    "span",
    "hr",
];

/// Alignment and indentation, which the editor keeps as classes rather than styles.
const CLASSES: &[&str] = &[
    "ql-align-center",
    "ql-align-right",
    "ql-align-justify",
    "ql-indent-1",
    "ql-indent-2",
    "ql-indent-3",
    "ql-indent-4",
    "ql-indent-5",
    "ql-indent-6",
    "ql-indent-7",
    "ql-indent-8",
];

static CLEANER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    let blocks = ["p", "h1", "h2", "h3", "h4", "li", "blockquote", "pre"];
    let classes: HashSet<&str> = CLASSES.iter().copied().collect();
    builder
        .tags(TAGS.iter().copied().collect())
        .generic_attributes(HashSet::new())
        .tag_attributes(HashMap::from([
            ("a", HashSet::from(["href", "title", "target"])),
            ("img", HashSet::from(["src", "alt", "width", "height"])),
        ]))
        .allowed_classes(blocks.iter().map(|tag| (*tag, classes.clone())).collect())
        // No `javascript:` or `data:` addresses: images are linked, not embedded.
        .url_schemes(HashSet::from(["http", "https", "mailto", "tel"]))
        .link_rel(Some("noopener noreferrer"));
    builder
});

/// Cleans HTML for storing in a rich-text field, as Cadmus does with everything written to
/// one: only the formatting the editor makes is kept (paragraphs, headings, bold, italics,
/// underline, strike-through, lists, links, images, quotes, code, alignment). Scripts, styles,
/// event handlers, forms, frames and `javascript:` or `data:` addresses are removed.
///
/// Use it for HTML you store yourself, or before showing HTML that reached the database
/// some other way.
///
/// ```
/// let clean = cadmus::clean_html(r#"<p onclick="steal()">Hi <script>x()</script><b>there</b></p>"#);
/// assert_eq!(clean, "<p>Hi <b>there</b></p>");
/// ```
#[must_use]
pub fn clean_html(html: &str) -> String {
    CLEANER.clean(html).to_string()
}

/// The text of cleaned HTML, for one-line previews: tags dropped, block ends turned into
/// spaces, the usual entities decoded.
pub(crate) fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + end];
        let name = tag
            .trim_start_matches('/')
            .split([' ', '/'])
            .next()
            .unwrap_or("");
        if matches!(
            name,
            "p" | "br" | "li" | "h1" | "h2" | "h3" | "h4" | "blockquote" | "pre" | "hr"
        ) {
            text.push(' ');
        }
        rest = &rest[start + end + 1..];
    }
    text.push_str(rest);
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_formatting_and_drops_everything_else() {
        let html = concat!(
            r#"<h2 class="ql-align-center evil">Title</h2>"#,
            r#"<p style="color:red" onmouseover="x()">A <a href="https://e.com" target="_blank">link</a>"#,
            r#" and <a href="javascript:alert(1)">this</a>.</p>"#,
            r#"<img src="https://e.com/a.png" alt="A" onerror="x()"><img src="data:image/png;base64,AAAA">"#,
            r#"<ul><li class="ql-indent-1">one</li></ul><iframe src="https://e.com"></iframe>"#,
            r#"<script>alert(1)</script><style>p{}</style><form><input></form>"#,
        );
        assert_eq!(
            clean_html(html),
            concat!(
                r#"<h2 class="ql-align-center">Title</h2>"#,
                r#"<p>A <a href="https://e.com" target="_blank" rel="noopener noreferrer">link</a>"#,
                r#" and <a rel="noopener noreferrer">this</a>.</p>"#,
                r#"<img src="https://e.com/a.png" alt="A"><img>"#,
                r#"<ul><li class="ql-indent-1">one</li></ul>"#,
            )
        );
    }

    #[test]
    fn plain_text_reads_like_the_page() {
        assert_eq!(
            plain_text("<h1>Fish &amp; chips</h1><p>Hot<br>and&nbsp;<b>salty</b></p>"),
            "Fish & chips Hot and salty"
        );
        assert_eq!(plain_text("no tags"), "no tags");
        assert_eq!(plain_text(""), "");
    }
}
