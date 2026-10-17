//! A systemd unit file, read the way systemd reads the parts the deploy checks use.
//!
//! - `[Section]` lines open a section; `Key=value` lines are directives in it. A
//!   directive before the first header has the section `""`.
//! - Blank lines and lines starting with `#` or `;` are skipped. Leading and trailing
//!   [`WHITESPACE`] is trimmed from a line, a key and a value, as systemd's `strstrip`
//!   does (`src/shared/conf-parser.c`); another space (U+00A0) is part of the value.
//! - A line ending in `\` continues on the next line, the `\` replaced by a space. A
//!   comment line inside a continued directive is skipped, as systemd skips it.
//! - A key given more than once keeps every assignment in order; [`UnitFile::get_in`]
//!   reads the last, the one systemd uses for a single-valued key.
//!
//! The docs check in `kbf-it` reads the example units in `docs/deploy/*.md` with it,
//! and `kbf-deploy linux check-units` the rendered ones, so both read units one way.

/// One `Key=value` line, continuations joined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    /// The section's name without brackets, such as `Service`.
    pub section: String,
    pub key: String,
    pub value: String,
}

/// A unit file's directives, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitFile {
    directives: Vec<Directive>,
}

/// Why a unit file was refused. Line numbers count from 1.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnitError {
    /// A line that is neither a section header, a comment nor `Key=value`.
    #[error("line {0}: not [Section], a comment or Key=value")]
    NotDirective(usize),
}

/// systemd's `WHITESPACE` (`src/basic/string-util.h`): what it trims and splits on.
pub const WHITESPACE: [char; 4] = [' ', '\t', '\n', '\r'];

impl UnitFile {
    /// Read `text`.
    pub fn parse(text: &str) -> Result<UnitFile, UnitError> {
        let mut directives = Vec::new();
        let mut section = "";
        // The directive so far and its first line, while a line ends in `\`.
        let mut pending: Option<(String, usize)> = None;
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim_matches(WHITESPACE);
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let (mut joined, first) = match pending.take() {
                Some(p) => p,
                None if line.is_empty() => continue,
                None => {
                    if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                        section = name;
                        continue;
                    }
                    (String::new(), i + 1)
                }
            };
            match line.strip_suffix('\\') {
                Some(head) => {
                    joined.push_str(head);
                    joined.push(' ');
                    pending = Some((joined, first));
                }
                None => {
                    joined.push_str(line);
                    directives.push(directive(section, &joined, first)?);
                }
            }
        }
        if let Some((joined, first)) = pending {
            directives.push(directive(section, &joined, first)?);
        }
        Ok(UnitFile { directives })
    }

    /// Every directive, in file order.
    pub fn directives(&self) -> &[Directive] {
        &self.directives
    }

    /// `drop_in`'s directives after this file's, as systemd applies a drop-in: each of
    /// its assignments comes after every one of the unit's own, so a single-valued key
    /// reads the drop-in's and an empty value there resets a list the unit built.
    pub fn extend(&mut self, drop_in: UnitFile) {
        self.directives.extend(drop_in.directives);
    }

    /// The value of the last `key=` line in any section.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.last(|d| d.key == key)
    }

    /// The value of the last `key=` line in `[section]`.
    pub fn get_in(&self, section: &str, key: &str) -> Option<&str> {
        self.last(|d| d.section == section && d.key == key)
    }

    /// The words of the last `ExecStart=`, split at whitespace; empty when there is none.
    pub fn exec_start(&self) -> Vec<&str> {
        self.get("ExecStart")
            .map(|v| v.split_whitespace().collect())
            .unwrap_or_default()
    }

    /// Every value given as `--<name>=<value>` in [`UnitFile::exec_start`], in order.
    pub fn flag_values(&self, name: &str) -> Vec<&str> {
        let prefix = format!("--{name}=");
        self.exec_start()
            .into_iter()
            .filter_map(|w| w.strip_prefix(prefix.as_str()))
            .collect()
    }

    /// The last value given as `--<name>=<value>`, as a command-line parser that lets a
    /// later flag override an earlier one reads it.
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flag_values(name).pop()
    }

    fn last(&self, pick: impl Fn(&Directive) -> bool) -> Option<&str> {
        self.directives
            .iter()
            .rev()
            .find(|d| pick(d))
            .map(|d| d.value.as_str())
    }
}

/// The line numbers (from 1) of the comment lines inside a continued directive: a `#`
/// or `;` line after a line ending in `\`, before the directive's last line.
///
/// systemd 241 and later skip such a line and go on joining (systemd.syntax(7), "When
/// a comment line or lines follow a line ending with a backslash, the comment block is
/// ignored"); 240 skips one only at the start of the line; 239 and older join the
/// comment into the value, so its words become arguments of an `ExecStart=`.
pub fn comments_in_continuations(text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut continued = false;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with(';') {
            if continued {
                found.push(i + 1);
            }
            continue;
        }
        continued = line.ends_with('\\');
    }
    found
}

fn directive(section: &str, line: &str, number: usize) -> Result<Directive, UnitError> {
    let (key, value) = line
        .split_once('=')
        .filter(|(k, _)| !k.trim_matches(WHITESPACE).is_empty())
        .ok_or(UnitError::NotDirective(number))?;
    Ok(Directive {
        section: section.to_owned(),
        key: key.trim_matches(WHITESPACE).to_owned(),
        value: value.trim_matches(WHITESPACE).to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: &str = "\
# a comment
[Unit]
Description=b

[Service]
; another comment
Environment=X=1
MemoryMin=2G
MemoryMin=3G
ExecStart=/usr/local/bin/b \\
    --node-id=n1 \\
# a comment inside the command line
    --image=docker://a\\
    --image=docker://b
[Slice]
MemoryMin=4G
";

    /// Catches a comment or `[Section]` header taken as a directive, `Environment=X=1`
    /// split at the wrong `=`, a directive filed under the wrong section, and a
    /// continuation joined without the space systemd puts in place of the `\` (so
    /// `docker://a\` and the next flag would run together).
    #[test]
    fn directives_keep_their_section_and_continuations_join_with_a_space() {
        let u = UnitFile::parse(UNIT).expect("unit");
        let keys: Vec<_> = u
            .directives()
            .iter()
            .map(|d| format!("{}.{}", d.section, d.key))
            .collect();
        assert_eq!(
            keys,
            [
                "Unit.Description",
                "Service.Environment",
                "Service.MemoryMin",
                "Service.MemoryMin",
                "Service.ExecStart",
                "Slice.MemoryMin"
            ]
        );
        assert_eq!(u.get("Environment"), Some("X=1"));
        assert_eq!(
            u.exec_start(),
            [
                "/usr/local/bin/b",
                "--node-id=n1",
                "--image=docker://a",
                "--image=docker://b"
            ]
        );
    }

    /// Catches reading the first assignment where systemd uses the last, and a section
    /// lookup that reads another section's key.
    #[test]
    fn the_last_assignment_in_the_section_wins() {
        let u = UnitFile::parse(UNIT).expect("unit");
        assert_eq!(u.get_in("Service", "MemoryMin"), Some("3G"));
        assert_eq!(u.get_in("Slice", "MemoryMin"), Some("4G"));
        assert_eq!(u.get("MemoryMin"), Some("4G"));
        assert_eq!(u.get_in("Unit", "MemoryMin"), None);
        assert_eq!(u.get("Nothing"), None);
    }

    /// Catches a repeated flag read as its first value (a later `--image=` or override
    /// would go unseen), and a flag matched by prefix (`--image` for `--image-x`).
    #[test]
    fn flags_are_read_from_exec_start_last_one_winning() {
        let u = UnitFile::parse(UNIT).expect("unit");
        assert_eq!(u.flag_values("image"), ["docker://a", "docker://b"]);
        assert_eq!(u.flag("image"), Some("docker://b"));
        assert_eq!(u.flag("node"), None);
        let none = UnitFile::parse("[Slice]\nMemoryMin=1G\n").expect("unit");
        assert!(none.exec_start().is_empty());
        assert_eq!(none.flag("node-id"), None);
    }

    /// Catches a garbled line (a lost `=`, an empty key) read as a directive with an
    /// empty value instead of refused, and a file ending inside a continued line losing
    /// that directive.
    #[test]
    fn a_line_that_is_not_a_directive_is_refused() {
        assert_eq!(
            UnitFile::parse("[Service]\nMemoryMin 1G\n"),
            Err(UnitError::NotDirective(2))
        );
        assert_eq!(
            UnitFile::parse("[Service]\n\n=1\n"),
            Err(UnitError::NotDirective(3))
        );
        let e = UnitError::NotDirective(2).to_string();
        assert_eq!(e, "line 2: not [Section], a comment or Key=value");
        let open = UnitFile::parse("[Service]\nExecStart=/bin/a \\\n  --b \\\n").expect("unit");
        assert_eq!(open.exec_start(), ["/bin/a", "--b"]);
        assert_eq!(
            UnitFile::parse("[Service]\nExecStart \\\n  --b\n"),
            Err(UnitError::NotDirective(2))
        );
    }
}
