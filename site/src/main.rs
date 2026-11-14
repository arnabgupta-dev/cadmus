//! Builds the Cadmus website: the landing page, every page of `cli/docs` as HTML, a search
//! index, and `llms.txt` for AI tools.
//!
//! ```sh
//! cargo run -p cadmus-site -- _site
//! ```
//!
//! Every link is relative, so the site works at any address, such as a GitHub Pages project
//! page under `/cadmus/`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde_json::json;

/// Where the site is published; used only where an absolute address is required.
const SITE_URL: &str = "https://arnabgupta-dev.github.io/cadmus";
const REPOSITORY: &str = "https://github.com/arnabgupta-dev/cadmus";

struct Page {
    slug: String,
    title: String,
    section: String,
    order: u32,
    summary: String,
    body: String,
}

struct Rendered {
    html: String,
    /// `(level, id, text)` for the headings under the title.
    toc: Vec<(u8, String, String)>,
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::args()
        .nth(1)
        .map_or_else(|| root.join("../_site"), PathBuf::from);
    let docs = root.join("../cli/docs");
    let version = version(&root.join("../Cargo.toml"));
    let pages = read_pages(&docs);

    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(out.join("docs")).expect("create the output folder");
    copy_dir(&root.join("assets"), &out.join("assets"));

    let landing = std::fs::read_to_string(root.join("templates/index.html")).unwrap();
    write(
        &out.join("index.html"),
        &fill(&landing, &[("version", &version)]),
    );
    let template = std::fs::read_to_string(root.join("templates/page.html")).unwrap();

    let mut search = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let rendered = render(&page.body);
        let prev = i.checked_sub(1).map(|j| &pages[j]);
        let next = pages.get(i + 1);
        let html = fill(
            &template,
            &[
                ("title", &escape(&page.title)),
                ("summary", &escape(&page.summary)),
                ("section", &escape(&page.section)),
                ("slug", &page.slug),
                ("version", &version),
                ("nav", &nav(&pages, &page.slug)),
                ("toc", &toc(&rendered.toc)),
                ("body", &rendered.html),
                ("pager", &pager(prev, next)),
                (
                    "edit",
                    &format!("{REPOSITORY}/edit/main/cli/docs/{}.md", page.slug),
                ),
                ("canonical", &format!("{SITE_URL}/docs/{}.html", page.slug)),
            ],
        );
        write(&out.join(format!("docs/{}.html", page.slug)), &html);
        // The Markdown too, for tools that read it (see llms.txt).
        write(
            &out.join(format!("docs/{}.md", page.slug)),
            &format!("# {}\n\n{}", page.title, page.body),
        );
        for (heading, text, anchor) in sections(page) {
            search.push(json!({
                "page": page.title,
                "heading": heading,
                "url": format!("{}.html{}", page.slug, anchor),
                "text": plain(&text),
            }));
        }
    }
    write(
        &out.join("docs/search.json"),
        &serde_json::to_string(&search).unwrap(),
    );
    let first = pages.first().expect("at least one page");
    write(
        &out.join("docs/index.html"),
        &format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Cadmus documentation</title>\
             <meta http-equiv=\"refresh\" content=\"0; url={0}.html\">\
             <link rel=\"canonical\" href=\"{SITE_URL}/docs/{0}.html\">\
             <a href=\"{0}.html\">Cadmus documentation</a>",
            first.slug
        ),
    );
    write(&out.join("llms.txt"), &llms(&pages));
    write(&out.join("llms-full.txt"), &llms_full(&pages));
    write(
        &out.join("404.html"),
        &fill(
            &std::fs::read_to_string(root.join("templates/404.html")).unwrap(),
            &[],
        ),
    );
    write(
        &out.join("robots.txt"),
        &format!("User-agent: *\nAllow: /\nSitemap: {SITE_URL}/sitemap.xml\n"),
    );
    write(&out.join("sitemap.xml"), &sitemap(&pages));
    // GitHub Pages must serve files and folders as they are.
    write(&out.join(".nojekyll"), "");
    println!("built {} pages into {}", pages.len(), out.display());
}

fn version(manifest: &Path) -> String {
    std::fs::read_to_string(manifest)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("0.0.0")
        .to_owned()
}

fn read_pages(dir: &Path) -> Vec<Page> {
    let mut pages: Vec<Page> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let slug = path.file_stem().unwrap().to_string_lossy().into_owned();
            let rest = text
                .strip_prefix("---\n")
                .unwrap_or_else(|| panic!("{slug}.md has no header"));
            let (header, body) = rest.split_once("\n---\n").unwrap();
            let field = |key: &str| {
                header
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key}:")))
                    .map(|v| v.trim().to_owned())
                    .unwrap_or_else(|| panic!("{slug}.md: no `{key}`"))
            };
            Page {
                title: field("title"),
                section: field("section"),
                order: field("order").parse().unwrap(),
                summary: field("summary"),
                body: body.trim_start_matches('\n').to_owned(),
                slug,
            }
        })
        .collect();
    pages.sort_by_key(|p| p.order);
    pages
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// Replaces `{{key}}` with each value.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (key, value) in values {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The anchor for a heading: lower case, words joined by dashes.
fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if (c.is_whitespace() || c == '-') && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// A link between pages: `models.md#x` becomes `models.html#x`.
fn page_link(url: &str) -> Option<String> {
    if url.contains("://") || url.starts_with('#') || url.starts_with("mailto:") {
        return None;
    }
    let (file, anchor) = url.split_once('#').map_or((url, ""), |(f, a)| (f, a));
    let stem = file.strip_suffix(".md")?;
    Some(if anchor.is_empty() {
        format!("{stem}.html")
    } else {
        format!("{stem}.html#{anchor}")
    })
}

fn level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn render(markdown: &str) -> Rendered {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut events: Vec<Event> = Vec::new();
    let mut toc = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let mut heading: Option<(u8, Vec<Event>)> = None;
    for event in Parser::new_ext(markdown, options) {
        let event = match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest_url = page_link(&dest_url).map_or(dest_url, CowStr::from);
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
            }
            other => other,
        };
        match event {
            Event::Start(Tag::Heading { level: l, .. }) => heading = Some((level(l), Vec::new())),
            Event::End(TagEnd::Heading(_)) => {
                let (n, inner) = heading.take().expect("heading start");
                let text: String = inner
                    .iter()
                    .filter_map(|e| match e {
                        Event::Text(t) | Event::Code(t) => Some(t.to_string()),
                        _ => None,
                    })
                    .collect();
                let mut id = slugify(&text);
                let base = id.clone();
                let mut k = 2;
                while used.contains(&id) {
                    id = format!("{base}-{k}");
                    k += 1;
                }
                used.push(id.clone());
                if n == 2 || n == 3 {
                    toc.push((n, id.clone(), text));
                }
                let mut inner_html = String::new();
                html::push_html(&mut inner_html, inner.into_iter());
                events.push(Event::Html(CowStr::from(format!(
                    "<h{n} id=\"{id}\"><a class=\"anchor\" href=\"#{id}\" aria-label=\"Link to this section\">#</a>{inner_html}</h{n}>\n"
                ))));
            }
            other => match &mut heading {
                Some((_, inner)) => inner.push(other),
                None => events.push(other),
            },
        }
    }
    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    // Wide tables scroll on their own instead of widening the page.
    let out = out
        .replace("<table>", "<div class=\"table-wrap\"><table>")
        .replace("</table>", "</table></div>");
    Rendered { html: out, toc }
}

fn nav(pages: &[Page], current: &str) -> String {
    let mut out = String::new();
    let mut section = "";
    for page in pages {
        if page.section != section {
            if !section.is_empty() {
                out.push_str("</ul></div>");
            }
            section = &page.section;
            let _ = write!(
                out,
                "<div class=\"nav-group\"><p class=\"nav-title\">{}</p><ul>",
                escape(section)
            );
        }
        let current_attr = if page.slug == current {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(
            out,
            "<li><a href=\"{}.html\"{current_attr}>{}</a></li>",
            page.slug,
            escape(&page.title)
        );
    }
    out.push_str("</ul></div>");
    out
}

fn toc(entries: &[(u8, String, String)]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("<p class=\"toc-title\">On this page</p><ul>");
    for (n, id, text) in entries {
        let _ = write!(
            out,
            "<li class=\"toc-{n}\"><a href=\"#{id}\">{}</a></li>",
            escape(text)
        );
    }
    out.push_str("</ul>");
    out
}

fn pager(prev: Option<&Page>, next: Option<&Page>) -> String {
    let mut out = String::new();
    if let Some(p) = prev {
        let _ = write!(
            out,
            "<a class=\"pager-prev\" href=\"{}.html\"><span>Previous</span>{}</a>",
            p.slug,
            escape(&p.title)
        );
    } else {
        out.push_str("<span></span>");
    }
    if let Some(n) = next {
        let _ = write!(
            out,
            "<a class=\"pager-next\" href=\"{}.html\"><span>Next</span>{}</a>",
            n.slug,
            escape(&n.title)
        );
    }
    out
}

/// A page's `##` sections: heading, Markdown text, and the anchor to link to.
fn sections(page: &Page) -> Vec<(String, String, String)> {
    let mut out = vec![(page.title.clone(), String::new(), String::new())];
    let mut in_code = false;
    for line in page.body.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        }
        if !in_code && let Some(heading) = line.strip_prefix("## ") {
            let heading = heading.trim().to_owned();
            let anchor = format!("#{}", slugify(&heading.replace('`', "")));
            out.push((heading, String::new(), anchor));
            continue;
        }
        let text = &mut out.last_mut().unwrap().1;
        text.push_str(line);
        text.push('\n');
    }
    out.retain(|(_, text, _)| !text.trim().is_empty());
    out
}

/// Markdown reduced to plain words, for the search index.
fn plain(markdown: &str) -> String {
    let mut out = String::new();
    for line in markdown.lines() {
        if line.starts_with("```") || line.starts_with("|---") {
            continue;
        }
        let mut cleaned = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '`' | '*' | '|' | '#' | '>' => cleaned.push(' '),
                '[' => {}
                ']' => {
                    // Drop the `(target)` of a link, keeping its text.
                    if chars.peek() == Some(&'(') {
                        for d in chars.by_ref() {
                            if d == ')' {
                                break;
                            }
                        }
                    }
                }
                _ => cleaned.push(c),
            }
        }
        out.push_str(
            cleaned
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .as_str(),
        );
        out.push(' ');
    }
    let out = out.trim().to_owned();
    match out.char_indices().nth(600) {
        Some((i, _)) => format!("{}…", &out[..i]),
        None => out,
    }
}

fn llms(pages: &[Page]) -> String {
    let mut out = String::from(
        "# Cadmus\n\n> A batteries-included web framework for Rust. Each model (a SeaORM entity) \
         gets a database table, an admin site and a REST API with documentation, with accounts, \
         permissions and migrations built in. Install the `cadmus` command with \
         `cargo install cadmus-cli`; `cadmus mcp` serves these docs and project tools to AI \
         agents over MCP.\n\n- Crate: https://crates.io/crates/cadmus\n- API reference: \
         https://docs.rs/cadmus\n- Source: https://github.com/arnabgupta-dev/cadmus\n- All \
         pages in one file: https://arnabgupta-dev.github.io/cadmus/llms-full.txt\n",
    );
    let mut section = "";
    for page in pages {
        if page.section != section {
            section = &page.section;
            let _ = write!(out, "\n## {section}\n\n");
        }
        let _ = writeln!(
            out,
            "- [{}]({SITE_URL}/docs/{}.md): {}",
            page.title, page.slug, page.summary
        );
    }
    out
}

fn llms_full(pages: &[Page]) -> String {
    let mut out = String::from("# Cadmus documentation\n\n");
    for page in pages {
        let _ = write!(out, "\n\n---\n\n# {}\n\n{}", page.title, page.body);
    }
    out
}

fn sitemap(pages: &[Page]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    let _ = writeln!(out, "  <url><loc>{SITE_URL}/</loc></url>");
    for page in pages {
        let _ = writeln!(
            out,
            "  <url><loc>{SITE_URL}/docs/{}.html</loc></url>",
            page.slug
        );
    }
    out.push_str("</urlset>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_get_anchors_and_page_links_point_at_html() {
        let rendered =
            render("## Two words\n\nSee [models](models.md#field-types).\n\n## Two words\n");
        assert!(rendered.html.contains("<h2 id=\"two-words\">"));
        assert!(rendered.html.contains("<h2 id=\"two-words-2\">"));
        assert!(rendered.html.contains("href=\"models.html#field-types\""));
        assert_eq!(rendered.toc.len(), 2);
    }

    #[test]
    fn slugs_and_plain_text() {
        assert_eq!(
            slugify("Logging in - tokens and API keys"),
            "logging-in-tokens-and-api-keys"
        );
        assert_eq!(
            plain("See [the docs](x.md) and `code`."),
            "See the docs and code ."
        );
    }
}
