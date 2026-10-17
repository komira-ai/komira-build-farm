//! The value of a `subject/<operation>` row: what the `action usage:` line says the
//! operation is ([`Subject`]), written with the submission so a process that takes the
//! operation up logs the same line its writer would have.
//!
//! A version byte (1), the shape key (8 bytes), the configuration hash (a presence byte,
//! then a text), the target (a presence byte, then its kind: 0 target, 1 anon, 2 bxl,
//! and its label as a text), whether the action is a test run (0 or 1), then the mnemonic
//! and the target hint (each a presence byte and a text). A text is a 4-byte length,
//! big-endian, then the bytes. A row of another
//! version is refused, not guessed at.

use kbf_front::ActionLabel;

use crate::action_usage::{ShapeKey, Subject, Target, TargetKind};

const VERSION: u8 = 1;

/// `subject` as a row value.
pub(super) fn encode(subject: &Subject) -> Vec<u8> {
    let mut row = vec![VERSION];
    row.extend(subject.key.bytes());
    text(&mut row, subject.cfg.as_deref());
    match &subject.target {
        None => row.push(0),
        Some(target) => {
            row.push(1);
            row.push(match target.kind {
                TargetKind::Target => 0,
                TargetKind::Anon => 1,
                TargetKind::Bxl => 2,
            });
            put(&mut row, &target.label);
        }
    }
    row.push(u8::from(subject.test));
    text(&mut row, subject.label.mnemonic.as_deref());
    text(&mut row, subject.label.target.as_deref());
    row
}

/// An optional text: a presence byte, then the text if present.
fn text(row: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None => row.push(0),
        Some(value) => {
            row.push(1);
            put(row, value);
        }
    }
}

/// A text: its length, then its bytes. Every text is a path or a label the front capped;
/// one past 4 GiB is cut.
fn put(row: &mut Vec<u8>, value: &str) {
    let bytes = &value.as_bytes()[..value.len().min(u32::MAX as usize)];
    row.extend((bytes.len() as u32).to_be_bytes());
    row.extend(bytes);
}

/// A row value as a [`Subject`], or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Subject, &'static str> {
    let mut at = Reader(row);
    if at.take(1)? != [VERSION] {
        return Err("not a version 1 subject");
    }
    let key = ShapeKey::from_bytes(at.array()?);
    let cfg = at.optional()?;
    let target = match at.take(1)? {
        [0] => None,
        [1] => {
            let kind = match at.take(1)? {
                [0] => TargetKind::Target,
                [1] => TargetKind::Anon,
                [2] => TargetKind::Bxl,
                _ => return Err("not a kind of target"),
            };
            Some(Target {
                kind,
                label: at.text()?,
            })
        }
        _ => return Err("not a presence byte"),
    };
    let test = match at.take(1)? {
        [0] => false,
        [1] => true,
        _ => return Err("not a test flag"),
    };
    let label = ActionLabel {
        mnemonic: at.optional()?,
        target: at.optional()?,
    };
    if !at.0.is_empty() {
        return Err("bytes after the label");
    }
    Ok(Subject {
        key,
        cfg,
        target,
        test,
        label,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let (head, rest) = self.0.split_at_checked(n).ok_or("the row ends early")?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let head = self.take(N)?;
        Ok(<[u8; N]>::try_from(head).expect("take returns N bytes"))
    }

    fn text(&mut self) -> Result<String, &'static str> {
        let len = u32::from_be_bytes(self.array()?) as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| "a text is not UTF-8")
    }

    fn optional(&mut self) -> Result<Option<String>, &'static str> {
        match self.take(1)? {
            [0] => Ok(None),
            [1] => self.text().map(Some),
            _ => Err("not a presence byte"),
        }
    }
}
