//! HTML that is escaped by construction.
//!
//! [`Html`] is the only way a page writes markup, and it has no method that writes a
//! caller's string unescaped: text and attribute values always go through [`escape`],
//! element and attribute names are `&'static str` literals checked against the
//! allowlists below (no `script`, no `style`, no `on*` handler, no `style=`), and a URL
//! attribute takes an [`Href`], which only a root-relative URL makes. The one place
//! that writes raw text is [`Html::raw`], private to this module, which the builder
//! uses for the doctype and for the names it has just checked. A page that needs
//! anything else needs a change here, which is the reviewed escape hatch.

/// Element names a page may write. `script`, `style`, `iframe`, `object`, `embed`,
/// `base` and `svg` are left out on purpose.
const TAGS: &[&str] = &[
    "a", "body", "code", "dd", "details", "div", "dl", "dt", "footer", "form", "h1", "h2", "head",
    "header", "html", "input", "label", "li", "link", "main", "meta", "nav", "ol", "p", "pre",
    "section", "small", "span", "summary", "table", "tbody", "td", "th", "thead", "time", "title",
    "tr", "ul",
];

/// Elements written without a closing tag.
const VOID: &[&str] = &["input", "link", "meta"];

/// Attribute names that take escaped text. No `style` and no event handler.
const TEXT_ATTRS: &[&str] = &[
    "aria-current",
    "aria-label",
    "autocomplete",
    "charset",
    "class",
    "content",
    "datetime",
    "for",
    "id",
    "lang",
    "method",
    "name",
    "placeholder",
    "rel",
    "required",
    "scope",
    "spellcheck",
    "title",
    "type",
    "value",
];

/// Attribute names that take an [`Href`].
const URL_ATTRS: &[&str] = &["action", "href"];

/// `s` with `&`, `<`, `>`, `"` and `'` written as character references, so it is
/// text in an element body and in a quoted attribute value alike.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Whether `c` is refused anywhere in a URL a page links to or reads: a backslash
/// (a WHATWG URL client reads it as `/` in a path), whitespace or a control
/// character (browsers strip or rewrite them before they parse a URL).
pub(crate) fn refused_in_url(c: char) -> bool {
    c == '\\' || c.is_whitespace() || c.is_control()
}

/// A URL a page may link to: root-relative (one leading `/`), so it names this
/// server, never another host, and never a `javascript:` or `data:` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Href(String);

impl Href {
    /// `url` if it is root-relative: it starts with `/`, its second character is
    /// neither `/` nor `\` (either would make it name another host), and it holds no
    /// backslash, whitespace or control character, which browsers strip or rewrite
    /// before they parse a URL. `None` otherwise.
    pub(crate) fn root_relative(url: &str) -> Option<Self> {
        let rest = url.strip_prefix('/')?;
        let foreign = rest.starts_with('/') || url.chars().any(refused_in_url);
        (!foreign).then(|| Self(url.to_owned()))
    }

    /// The URL.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One attribute: a name from [`TEXT_ATTRS`] with text, or one from [`URL_ATTRS`]
/// with an [`Href`].
pub(crate) enum Attr<'a> {
    Text(&'static str, &'a str),
    Url(&'static str, &'a Href),
}

/// A text attribute.
pub(crate) const fn attr<'a>(name: &'static str, value: &'a str) -> Attr<'a> {
    Attr::Text(name, value)
}

/// A URL attribute.
pub(crate) const fn url<'a>(name: &'static str, value: &'a Href) -> Attr<'a> {
    Attr::Url(name, value)
}

/// A document being written.
#[derive(Debug, Default)]
pub(crate) struct Html {
    out: String,
}

impl Html {
    /// A document that starts with the HTML5 doctype.
    pub(crate) fn document() -> Self {
        let mut h = Self::default();
        h.raw("<!DOCTYPE html>\n");
        h
    }

    /// Writes `s` as text.
    pub(crate) fn text(&mut self, s: &str) -> &mut Self {
        let escaped = escape(s);
        self.raw(&escaped)
    }

    /// Writes element `tag` with `attrs`, its body written by `body`.
    ///
    /// # Panics
    /// `tag` is not in [`TAGS`], is a void element, or an attribute name is not in
    /// its allowlist: a page's own literal is wrong, which every page test renders.
    pub(crate) fn el(
        &mut self,
        tag: &'static str,
        attrs: &[Attr<'_>],
        body: impl FnOnce(&mut Self),
    ) -> &mut Self {
        assert!(!VOID.contains(&tag), "<{tag}> has no body: use void");
        self.open(tag, attrs);
        body(self);
        self.raw("</").raw(tag).raw(">")
    }

    /// Writes void element `tag` (one of [`VOID`]) with `attrs`.
    ///
    /// # Panics
    /// As [`Html::el`], and `tag` is not a void element.
    pub(crate) fn void(&mut self, tag: &'static str, attrs: &[Attr<'_>]) -> &mut Self {
        assert!(VOID.contains(&tag), "<{tag}> is not a void element: use el");
        self.open(tag, attrs)
    }

    /// `<a href=...>` around the text `label`.
    pub(crate) fn link(&mut self, href: &Href, label: &str) -> &mut Self {
        self.el("a", &[url("href", href)], |h| {
            h.text(label);
        })
    }

    /// The document written.
    pub(crate) fn finish(self) -> String {
        self.out
    }

    fn open(&mut self, tag: &'static str, attrs: &[Attr<'_>]) -> &mut Self {
        assert!(TAGS.contains(&tag), "<{tag}> is not an allowed element");
        self.raw("<").raw(tag);
        for a in attrs {
            let (name, value, allowed) = match a {
                Attr::Text(name, value) => (*name, *value, TEXT_ATTRS),
                Attr::Url(name, href) => (*name, href.as_str(), URL_ATTRS),
            };
            assert!(
                allowed.contains(&name),
                "{name}= is not an allowed attribute of its kind"
            );
            let value = escape(value);
            self.raw(" ").raw(name).raw("=\"").raw(&value).raw("\"");
        }
        self.raw(">")
    }

    /// The one unescaped write, private to this module.
    fn raw(&mut self, s: &str) -> &mut Self {
        self.out.push_str(s);
        self
    }
}

#[cfg(test)]
mod tests;
