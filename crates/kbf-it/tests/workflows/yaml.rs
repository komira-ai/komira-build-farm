//! One YAML document as a tree, built from `saphyr-parser` events.
//!
//! The tree keeps only what the workflow lint evaluates: scalars (text, and whether
//! they were plain), sequences and mappings, each with the span it came from. Aliases
//! are resolved to a copy of the anchored node. Everything the lint does not evaluate
//! is refused here rather than read some other way: a tag, a second document, an
//! alias to a node that is not finished, or more nodes than [`MAX_NODES`] (an alias
//! bomb).

use std::collections::BTreeMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};

/// Nodes one document may expand to, aliases included.
pub const MAX_NODES: usize = 100_000;

#[derive(Clone, Debug)]
pub enum Value {
    /// A scalar's text after unescaping and folding; `plain` is false for quoted and
    /// block scalars, which are always strings.
    Scalar {
        text: String,
        plain: bool,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Clone, Debug)]
pub struct Node {
    pub value: Value,
    /// Where the node starts and ends (for a collection, its opening token).
    pub span: Span,
}

impl Node {
    /// The 1-based line the node starts on.
    pub fn line(&self) -> usize {
        self.span.start.line()
    }

    /// The scalar's text, if the node is a scalar that is not null.
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::Scalar { text, plain } if !(*plain && is_null(text)) => Some(text),
            _ => None,
        }
    }

    /// True for a mapping with no entries (`{}`).
    pub fn is_empty_map(&self) -> bool {
        matches!(&self.value, Value::Map(m) if m.is_empty())
    }

    fn size(&self) -> usize {
        1 + match &self.value {
            Value::Scalar { .. } => 0,
            Value::Seq(items) => items.iter().map(Node::size).sum(),
            Value::Map(entries) => entries.iter().map(|(k, v)| k.size() + v.size()).sum(),
        }
    }
}

/// The plain scalars the YAML 1.2 core schema reads as null.
fn is_null(text: &str) -> bool {
    matches!(text, "" | "~" | "null" | "Null" | "NULL")
}

/// A collection whose end event has not arrived yet.
struct Open {
    anchor: usize,
    span: Span,
    items: Vec<Node>,
    is_map: bool,
}

impl Open {
    fn new(anchor: usize, span: Span, is_map: bool) -> Self {
        Open {
            anchor,
            span,
            items: Vec::new(),
            is_map,
        }
    }

    fn close(self) -> Result<Node, String> {
        let value = if self.is_map {
            if !self.items.len().is_multiple_of(2) {
                return Err(at(self.span, "a mapping key has no value"));
            }
            let mut it = self.items.into_iter();
            let mut entries = Vec::new();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                entries.push((k, v));
            }
            Value::Map(entries)
        } else {
            Value::Seq(self.items)
        };
        Ok(Node {
            value,
            span: self.span,
        })
    }
}

fn at(span: Span, what: &str) -> String {
    format!("line {}: {what}", span.start.line())
}

fn refuse_tag(tag: Option<&Tag>, span: Span) -> Result<(), String> {
    match tag {
        Some(t) => Err(at(span, &format!("a YAML tag (`{t}`) is not evaluated"))),
        None => Ok(()),
    }
}

/// Parses `text` as exactly one YAML document.
pub fn load(text: &str) -> Result<Node, String> {
    let mut anchors: BTreeMap<usize, Node> = BTreeMap::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut docs: Vec<Node> = Vec::new();
    let mut budget = MAX_NODES;
    for event in Parser::new_from_str(text) {
        let (event, span) = event.map_err(|e| format!("line 0: not valid YAML: {e}"))?;
        // Each node costs one; an alias costs the size of the copy it makes.
        let (node, anchor, cost) = match event {
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd => continue,
            Event::Scalar(text, style, anchor, tag) => {
                refuse_tag(tag.as_deref(), span)?;
                let value = Value::Scalar {
                    text: text.into_owned(),
                    plain: style == ScalarStyle::Plain,
                };
                (Node { value, span }, anchor, 1)
            }
            Event::SequenceStart(anchor, tag) => {
                refuse_tag(tag.as_deref(), span)?;
                stack.push(Open::new(anchor, span, false));
                continue;
            }
            Event::MappingStart(anchor, tag) => {
                refuse_tag(tag.as_deref(), span)?;
                stack.push(Open::new(anchor, span, true));
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let open = stack
                    .pop()
                    .ok_or_else(|| at(span, "a collection ends that never started"))?;
                let anchor = open.anchor;
                (open.close()?, anchor, 1)
            }
            Event::Alias(id) => {
                let target = anchors
                    .get(&id)
                    .ok_or_else(|| at(span, "an alias names a node that is not finished"))?;
                (target.clone(), 0, target.size())
            }
        };
        budget = budget.checked_sub(cost).ok_or_else(|| {
            at(
                span,
                &format!("the document expands to more than {MAX_NODES} nodes"),
            )
        })?;
        if anchor != 0 {
            anchors.insert(anchor, node.clone());
        }
        match stack.last_mut() {
            Some(open) => open.items.push(node),
            None => docs.push(node),
        }
    }
    let mut docs = docs.into_iter();
    match (docs.next(), docs.next()) {
        (Some(doc), None) => Ok(doc),
        (None, _) => Err("line 0: the file holds no YAML document".to_owned()),
        (Some(_), Some(second)) => Err(format!(
            "line {}: a second YAML document is not evaluated",
            second.line()
        )),
    }
}
