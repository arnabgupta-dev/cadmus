//! The documentation, embedded from `docs/*.md`: what `cadmus mcp` serves to agents, and what
//! the website is built from.
//!
//! Each page starts with a header:
//!
//! ```text
//! ---
//! title: Models
//! section: Guides
//! order: 40
//! summary: Describe your data; every model gets a table, admin pages and API endpoints.
//! ---
//! ```

include!(concat!(env!("OUT_DIR"), "/docs.rs"));

/// One page of the documentation.
pub(crate) struct Page {
    /// The file name without `.md`, e.g. `models`.
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) section: String,
    pub(crate) order: u32,
    pub(crate) summary: String,
    /// The Markdown after the header.
    pub(crate) body: &'static str,
}

/// Every page, in reading order.
pub(crate) fn pages() -> Vec<Page> {
    let mut pages: Vec<Page> = FILES
        .iter()
        .filter_map(|(name, text)| parse(name, text))
        .collect();
    pages.sort_by_key(|p| p.order);
    pages
}

/// The page with this slug (`models`, or `models.md`).
pub(crate) fn page(slug: &str) -> Option<Page> {
    let slug = slug.trim().trim_end_matches(".md");
    pages().into_iter().find(|p| p.slug == slug)
}

fn parse(name: &str, text: &'static str) -> Option<Page> {
    let rest = text.strip_prefix("---\n")?;
    let (header, body) = rest.split_once("\n---\n")?;
    let field = |key: &str| {
        header
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .map(|value| value.trim().to_owned())
    };
    Some(Page {
        slug: name.trim_end_matches(".md").to_owned(),
        title: field("title")?,
        section: field("section")?,
        order: field("order")?.parse().ok()?,
        summary: field("summary").unwrap_or_default(),
        body: body.trim_start_matches('\n'),
    })
}

/// A part of a page under one `##` heading.
pub(crate) struct Hit {
    pub(crate) page: String,
    pub(crate) heading: String,
    pub(crate) text: String,
}

/// The sections that best match `query`, best first.
pub(crate) fn search(query: &str, limit: usize) -> Vec<Hit> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|t| t.len() > 1)
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored = Vec::new();
    for page in pages() {
        for (heading, text) in sections(&page) {
            let heading_lower = heading.to_lowercase();
            let text_lower = text.to_lowercase();
            let mut score = 0;
            for term in &terms {
                score += heading_lower.matches(term.as_str()).count() * 5;
                score += text_lower.matches(term.as_str()).count();
            }
            // Every term should appear somewhere.
            if score > 0
                && terms
                    .iter()
                    .all(|t| text_lower.contains(t.as_str()) || heading_lower.contains(t.as_str()))
            {
                scored.push((
                    score,
                    Hit {
                        page: page.slug.clone(),
                        heading,
                        text,
                    },
                ));
            }
        }
    }
    scored.sort_by_key(|hit| std::cmp::Reverse(hit.0));
    scored.into_iter().take(limit).map(|(_, hit)| hit).collect()
}

/// A page split at its `##` headings; the text before the first one is under the title.
pub(crate) fn sections(page: &Page) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![(page.title.clone(), String::new())];
    let mut in_code = false;
    for line in page.body.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        }
        if !in_code && let Some(heading) = line.strip_prefix("## ") {
            out.push((heading.trim().to_owned(), String::new()));
            continue;
        }
        let current = &mut out.last_mut().expect("never empty").1;
        current.push_str(line);
        current.push('\n');
    }
    out.retain(|(_, text)| !text.trim().is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_has_a_header_and_a_unique_order() {
        let pages = pages();
        assert_eq!(
            pages.len(),
            FILES.len(),
            "a page in docs/ has a broken header"
        );
        let mut orders: Vec<u32> = pages.iter().map(|p| p.order).collect();
        orders.dedup();
        assert_eq!(orders.len(), pages.len(), "two pages share an order");
        assert!(pages.iter().all(|p| !p.summary.is_empty()));
    }

    #[test]
    fn links_between_pages_point_at_pages_that_exist() {
        let pages = pages();
        for page in &pages {
            let mut rest = page.body;
            while let Some(i) = rest.find("](") {
                rest = &rest[i + 2..];
                let link = rest.split(')').next().unwrap_or_default();
                if link.starts_with("http") || link.starts_with('#') || link.starts_with("mailto:")
                {
                    continue;
                }
                let file = link.split('#').next().unwrap_or_default();
                assert!(
                    file.ends_with(".md") && pages.iter().any(|p| format!("{}.md", p.slug) == file),
                    "{}: link to `{link}` does not name a page",
                    page.slug
                );
            }
        }
    }

    #[test]
    fn search_finds_sections() {
        let hits = search("api key", 5);
        assert!(!hits.is_empty());
        assert!(hits[0].heading.to_lowercase().contains("key") || hits[0].text.contains("key"));
    }
}
