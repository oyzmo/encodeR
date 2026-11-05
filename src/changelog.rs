//! CHANGELOG.md, compiled in and converted to the AppStream-style markup
//! that `adw::AboutDialog::set_release_notes` accepts (p, ul, li, em, code).

pub const MARKDOWN: &str = include_str!("../CHANGELOG.md");

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `## 1.2.3` → `<p><em>1.2.3</em></p>`, `- item` → `<li>`, other text → `<p>`.
/// The `# Title` line is dropped (the page has its own title).
pub fn to_release_notes(md: &str) -> String {
    let mut out = String::new();
    let mut in_list = false;
    for line in md.lines().map(str::trim) {
        if let Some(item) = line.strip_prefix("- ") {
            if !in_list {
                out.push_str("<ul>");
                in_list = true;
            }
            out.push_str(&format!("<li>{}</li>", escape(item)));
            continue;
        }
        if in_list {
            out.push_str("</ul>");
            in_list = false;
        }
        if line.is_empty() || (line.starts_with("# ")) {
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            out.push_str(&format!("<p><em>{}</em></p>", escape(h)));
        } else {
            out.push_str(&format!("<p>{}</p>", escape(line)));
        }
    }
    if in_list {
        out.push_str("</ul>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_headings_lists_and_escapes() {
        let md = "# Changelog\n\n## 0.2.0\n\n- A & B\n- <x>\n\nNote\n## 0.1.0\n- C\n";
        assert_eq!(
            to_release_notes(md),
            "<p><em>0.2.0</em></p><ul><li>A &amp; B</li><li>&lt;x&gt;</li></ul>\
             <p>Note</p><p><em>0.1.0</em></p><ul><li>C</li></ul>"
        );
    }

    #[test]
    fn changelog_covers_current_version() {
        let v = env!("CARGO_PKG_VERSION");
        assert!(
            MARKDOWN.contains(&format!("## {v}\n")),
            "CHANGELOG.md has no entry for {v}"
        );
    }
}
