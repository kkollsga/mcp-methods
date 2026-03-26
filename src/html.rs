use pyo3::prelude::*;
use regex::Regex;

/// Convert HTML to clean, readable text optimized for LLM consumption.
///
/// Strips tags, converts headings to markdown `#` prefixes, list items to
/// `- ` bullets, bold to `**text**`, images to `[image: alt]`, tables to
/// tab-separated text, and decodes HTML entities.
#[pyfunction]
pub fn html_to_text(html: &str) -> String {
    html_to_text_impl(html)
}

/// Core implementation shared by the standalone function and read_file transform.
pub fn html_to_text_impl(html: &str) -> String {
    let mut text = html.to_string();

    // ── 1. Remove non-content sections ────────────────────────────────
    let re_head = Regex::new(r"(?is)<head[\s>].*?</head>").unwrap();
    text = re_head.replace_all(&text, "").to_string();
    let re_script = Regex::new(r"(?is)<script[\s>].*?</script>").unwrap();
    text = re_script.replace_all(&text, "").to_string();
    let re_style = Regex::new(r"(?is)<style[\s>].*?</style>").unwrap();
    text = re_style.replace_all(&text, "").to_string();
    let re_comment = Regex::new(r"(?s)<!--.*?-->").unwrap();
    text = re_comment.replace_all(&text, "").to_string();

    // ── 2. Headings → markdown # prefix ───────────────────────────────
    for level in 1..=6usize {
        let pattern = format!(r"(?is)<h{0}\b[^>]*>(.*?)</h{0}\s*>", level);
        let re = Regex::new(&pattern).unwrap();
        let prefix = "#".repeat(level);
        text = re
            .replace_all(&text, |caps: &regex::Captures| {
                format!("\n{} {}\n", prefix, &caps[1])
            })
            .to_string();
    }

    // ── 3. List items → "- " prefix ──────────────────────────────────
    let re_li = Regex::new(r"(?i)<li\b[^>]*>").unwrap();
    text = re_li.replace_all(&text, "\n- ").to_string();

    // ── 4. Bold / strong → **text** ──────────────────────────────────
    let re_b = Regex::new(r"(?is)<b\b[^>]*>(.*?)</b\s*>").unwrap();
    text = re_b.replace_all(&text, "**$1**").to_string();
    let re_strong = Regex::new(r"(?is)<strong\b[^>]*>(.*?)</strong\s*>").unwrap();
    text = re_strong.replace_all(&text, "**$1**").to_string();

    // ── 5. Images → [image: alt_text] ────────────────────────────────
    let re_img_alt = Regex::new(r#"(?i)<img\b[^>]*\balt=["']([^"']*)["'][^>]*/?\s*>"#).unwrap();
    text = re_img_alt.replace_all(&text, "[image: $1]").to_string();
    let re_img_no_alt = Regex::new(r"(?i)<img\b[^>]*/?\s*>").unwrap();
    text = re_img_no_alt.replace_all(&text, "").to_string();

    // ── 6. Table cells → tab separator ───────────────────────────────
    let re_cell = Regex::new(r"(?i)<(td|th)\b[^>]*>").unwrap();
    text = re_cell.replace_all(&text, "\t").to_string();

    // ── 7. <br> → newline ────────────────────────────────────────────
    let re_br = Regex::new(r"(?i)<br\b[^>]*/?\s*>").unwrap();
    text = re_br.replace_all(&text, "\n").to_string();

    // ── 8. Block-level tags → newline ────────────────────────────────
    let re_block_open = Regex::new(
        r"(?i)<(p|div|section|article|table|tr|ul|ol|blockquote|header|footer|nav|main|aside|figure|figcaption|details|summary|dl|dt|dd|pre|address)\b[^>]*>",
    )
    .unwrap();
    text = re_block_open.replace_all(&text, "\n").to_string();
    let re_block_close = Regex::new(
        r"(?i)</(p|div|section|article|table|tr|ul|ol|blockquote|header|footer|nav|main|aside|figure|figcaption|details|summary|dl|dt|dd|pre|address|h[1-6])>",
    )
    .unwrap();
    text = re_block_close.replace_all(&text, "\n").to_string();

    // ── 9. Strip remaining HTML tags ─────────────────────────────────
    let re_tags = Regex::new(r"<[^>]+>").unwrap();
    text = re_tags.replace_all(&text, "").to_string();

    // ── 10. Decode HTML entities ─────────────────────────────────────
    text = decode_entities(&text);

    // ── 11. Collapse whitespace ──────────────────────────────────────
    // Horizontal whitespace → single space (preserve newlines)
    let re_hspace = Regex::new(r"[^\S\n]+").unwrap();
    text = re_hspace.replace_all(&text, " ").to_string();
    // 3+ consecutive newlines → 2
    let re_blanks = Regex::new(r"\n{3,}").unwrap();
    text = re_blanks.replace_all(&text, "\n\n").to_string();
    // Trim each line
    let trimmed: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    text = trimmed.join("\n");

    text.trim().to_string()
}

/// Decode common HTML entities and numeric character references.
fn decode_entities(text: &str) -> String {
    let mut s = text.to_string();

    // Named entities (decode &amp; LAST to avoid double-decoding)
    let entities: &[(&str, &str)] = &[
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        // Punctuation & symbols
        ("&mdash;", "\u{2014}"),
        ("&ndash;", "\u{2013}"),
        ("&laquo;", "\u{00AB}"),
        ("&raquo;", "\u{00BB}"),
        ("&hellip;", "\u{2026}"),
        ("&bull;", "\u{2022}"),
        ("&lsquo;", "\u{2018}"),
        ("&rsquo;", "\u{2019}"),
        ("&ldquo;", "\u{201C}"),
        ("&rdquo;", "\u{201D}"),
        ("&copy;", "\u{00A9}"),
        ("&reg;", "\u{00AE}"),
        ("&trade;", "\u{2122}"),
        ("&sect;", "\u{00A7}"),
        ("&para;", "\u{00B6}"),
        ("&deg;", "\u{00B0}"),
        ("&times;", "\u{00D7}"),
        ("&divide;", "\u{00F7}"),
        ("&frac12;", "\u{00BD}"),
        ("&frac14;", "\u{00BC}"),
        ("&frac34;", "\u{00BE}"),
        ("&plusmn;", "\u{00B1}"),
        ("&micro;", "\u{00B5}"),
        // European / Scandinavian letters
        ("&aelig;", "\u{00E6}"),
        ("&AElig;", "\u{00C6}"),
        ("&oslash;", "\u{00F8}"),
        ("&Oslash;", "\u{00D8}"),
        ("&aring;", "\u{00E5}"),
        ("&Aring;", "\u{00C5}"),
        ("&auml;", "\u{00E4}"),
        ("&Auml;", "\u{00C4}"),
        ("&ouml;", "\u{00F6}"),
        ("&Ouml;", "\u{00D6}"),
        ("&uuml;", "\u{00FC}"),
        ("&Uuml;", "\u{00DC}"),
        ("&szlig;", "\u{00DF}"),
        ("&ntilde;", "\u{00F1}"),
        ("&Ntilde;", "\u{00D1}"),
        ("&ccedil;", "\u{00E7}"),
        ("&Ccedil;", "\u{00C7}"),
        ("&eacute;", "\u{00E9}"),
        ("&Eacute;", "\u{00C9}"),
        ("&egrave;", "\u{00E8}"),
        ("&Egrave;", "\u{00C8}"),
        ("&ecirc;", "\u{00EA}"),
        ("&Ecirc;", "\u{00CA}"),
        ("&agrave;", "\u{00E0}"),
        ("&Agrave;", "\u{00C0}"),
        ("&aacute;", "\u{00E1}"),
        ("&Aacute;", "\u{00C1}"),
        ("&acirc;", "\u{00E2}"),
        ("&Acirc;", "\u{00C2}"),
        ("&iacute;", "\u{00ED}"),
        ("&Iacute;", "\u{00CD}"),
        ("&igrave;", "\u{00EC}"),
        ("&Igrave;", "\u{00CC}"),
        ("&ocirc;", "\u{00F4}"),
        ("&Ocirc;", "\u{00D4}"),
        ("&oacute;", "\u{00F3}"),
        ("&Oacute;", "\u{00D3}"),
        ("&ograve;", "\u{00F2}"),
        ("&Ograve;", "\u{00D2}"),
        ("&uacute;", "\u{00FA}"),
        ("&Uacute;", "\u{00DA}"),
        ("&ugrave;", "\u{00F9}"),
        ("&Ugrave;", "\u{00D9}"),
    ];
    for &(entity, replacement) in entities {
        s = s.replace(entity, replacement);
    }

    // Numeric: &#123;
    let re_dec = Regex::new(r"&#(\d+);").unwrap();
    s = re_dec
        .replace_all(&s, |caps: &regex::Captures| {
            caps[1]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .to_string();

    // Hex: &#x1F;
    let re_hex = Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap();
    s = re_hex
        .replace_all(&s, |caps: &regex::Captures| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .to_string();

    // &amp; decoded last to prevent double-decoding (&amp;lt; → &lt; not <)
    s = s.replace("&amp;", "&");

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_head() {
        let html =
            "<html><head><title>T</title><style>body{}</style></head><body>Hello</body></html>";
        let result = html_to_text_impl(html);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn test_headings() {
        let html = "<h1>Title</h1><h2>Sub</h2><p>Text</p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("# Title"));
        assert!(result.contains("## Sub"));
        assert!(result.contains("Text"));
    }

    #[test]
    fn test_list_items() {
        let html = "<ul><li>Alpha</li><li>Beta</li></ul>";
        let result = html_to_text_impl(html);
        assert!(result.contains("- Alpha"));
        assert!(result.contains("- Beta"));
    }

    #[test]
    fn test_bold() {
        let html = "<p>Hello <strong>world</strong> and <b>rust</b></p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("**world**"));
        assert!(result.contains("**rust**"));
    }

    #[test]
    fn test_images() {
        let html = r#"<img alt="logo" src="logo.png"><img src="spacer.gif">"#;
        let result = html_to_text_impl(html);
        assert!(result.contains("[image: logo]"));
        assert!(!result.contains("spacer"));
    }

    #[test]
    fn test_tables() {
        let html =
            "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Alice</td><td>30</td></tr></table>";
        let result = html_to_text_impl(html);
        assert!(result.contains("Name"));
        assert!(result.contains("Age"));
        assert!(result.contains("Alice"));
        assert!(result.contains("30"));
    }

    #[test]
    fn test_entities() {
        let html = "<p>&lt;tag&gt; &amp; &quot;quotes&quot; &#169; &#x00A7;</p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("<tag>"));
        assert!(result.contains("& \"quotes\""));
        assert!(result.contains("\u{00A9}")); // ©
        assert!(result.contains("\u{00A7}")); // §
    }

    #[test]
    fn test_double_encoded_entities() {
        let html = "<p>&amp;lt; should stay as &amp;lt;</p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("&lt;"));
    }

    #[test]
    fn test_script_style_removed() {
        let html =
            "<p>Before</p><script>alert('xss')</script><style>.a{color:red}</style><p>After</p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("alert"));
        assert!(!result.contains("color"));
    }

    #[test]
    fn test_comments_removed() {
        let html = "<p>A<!-- hidden -->B</p>";
        let result = html_to_text_impl(html);
        assert!(result.contains("AB") || result.contains("A B"));
        assert!(!result.contains("hidden"));
    }

    #[test]
    fn test_links_stripped() {
        let html = r#"<a href="https://example.com">click here</a>"#;
        let result = html_to_text_impl(html);
        assert!(result.contains("click here"));
        assert!(!result.contains("https://"));
    }

    #[test]
    fn test_whitespace_collapsed() {
        let html = "<p>  lots   of   spaces  </p>\n\n\n\n<p>after gap</p>";
        let result = html_to_text_impl(html);
        assert!(!result.contains("   "));
        // No more than 2 consecutive newlines
        assert!(!result.contains("\n\n\n"));
    }

    #[test]
    fn test_br_tags() {
        let html = "line1<br>line2<br/>line3<br />line4";
        let result = html_to_text_impl(html);
        assert!(result.contains("line1\nline2"));
        assert!(result.contains("line3"));
        assert!(result.contains("line4"));
    }
}
