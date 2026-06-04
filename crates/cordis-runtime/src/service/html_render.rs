//! Minimal safe HTML builder.
//!
//! `HtmlWriter` makes the safe path the default: [`text`](HtmlWriter::text)
//! automatically escapes special characters, while [`raw`](HtmlWriter::raw)
//! is an explicit opt-out for trusted content (static tags, attributes, etc.).

/// A minimal builder for safe HTML string construction.
///
/// Every method that accepts user-controlled content escapes it by default.
/// Use [`raw`](HtmlWriter::raw) only for hard-coded markup.
#[derive(Debug, Default)]
pub struct HtmlWriter {
    buf: String,
}

impl HtmlWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the writer and return the built HTML string.
    pub fn into_string(self) -> String {
        self.buf
    }

    /// Append **escaped** text.
    pub fn text(&mut self, s: &str) -> &mut Self {
        self.buf.push_str(&escape_html(s));
        self
    }

    /// Append **raw** (pre-escaped / trusted) markup.
    ///
    /// Use sparingly — every call should be auditable.  Static HTML tags and
    /// attributes are the primary intended use.
    pub fn raw(&mut self, s: &str) -> &mut Self {
        self.buf.push_str(s);
        self
    }

    /// Open a tag: `<tag>`.
    pub fn open_tag(&mut self, tag: &str) -> &mut Self {
        self.buf.push('<');
        self.buf.push_str(tag);
        self.buf.push('>');
        self
    }

    /// Close a tag: `</tag>`.
    pub fn close_tag(&mut self, tag: &str) -> &mut Self {
        self.buf.push_str("</");
        self.buf.push_str(tag);
        self.buf.push('>');
        self
    }

    /// A self-closing void element: `<tag>` (HTML5 style, no trailing slash).
    pub fn void_tag(&mut self, tag: &str) -> &mut Self {
        self.buf.push('<');
        self.buf.push_str(tag);
        self.buf.push('>');
        self
    }

    /// `<tag>escaped(content)</tag>`.
    pub fn text_element(&mut self, tag: &str, content: &str) -> &mut Self {
        self.open_tag(tag);
        self.text(content);
        self.close_tag(tag)
    }

    /// `<tag>raw(content)</tag>` — content is trusted / pre-escaped.
    pub fn raw_element(&mut self, tag: &str, content: &str) -> &mut Self {
        self.open_tag(tag);
        self.raw(content);
        self.close_tag(tag)
    }
}

/// Escape the five special HTML characters: `&`, `<`, `>`, `"`, `'`.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_special_chars() {
        let mut w = HtmlWriter::new();
        w.text("<script>alert('&')</script>");
        assert_eq!(
            w.into_string(),
            "&lt;script&gt;alert(&#39;&amp;&#39;)&lt;/script&gt;"
        );
    }

    #[test]
    fn raw_passes_through() {
        let mut w = HtmlWriter::new();
        w.raw("<br>");
        assert_eq!(w.into_string(), "<br>");
    }

    #[test]
    fn text_element_wraps_tag() {
        let mut w = HtmlWriter::new();
        w.text_element("p", "hello <world>");
        assert_eq!(w.into_string(), "<p>hello &lt;world&gt;</p>");
    }

    #[test]
    fn nested_elements() {
        let mut w = HtmlWriter::new();
        w.raw("<!doctype html>");
        w.open_tag("html");
        w.text_element("title", "My Title");
        w.close_tag("html");
        assert_eq!(
            w.into_string(),
            "<!doctype html><html><title>My Title</title></html>"
        );
    }
}
