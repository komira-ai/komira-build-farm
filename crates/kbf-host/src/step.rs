//! The steps a plan is made of, and the line each prints.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::path::HostPath;
use crate::user::RunUser;

/// Permission bits of a file or directory, at most `0o7777`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms(u32);

impl Perms {
    /// # Panics
    ///
    /// When `bits` has a bit above `0o7777` set. Kits pass literals, so in a `const` this
    /// is a compile error.
    pub const fn new(bits: u32) -> Perms {
        assert!(bits <= 0o7777, "permission bits are at most 0o7777");
        Perms(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Perms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

/// A numeric owner and group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// An external tool, named by its absolute path, with its arguments and the whole of
/// its environment: it starts with an empty environment plus `env`, never through a
/// shell. Its standard input is `stdin` (empty: it reads end of file at once).
///
/// Arguments are printed as they are, so a secret goes in `env` or `stdin`, never in
/// `args`: argv is world-readable while the tool runs, and printed lines are kept.
///
/// With `user`, the tool runs as that user, with the user's groups ([`crate::user`]);
/// only root can start it so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: HostPath,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    pub user: Option<RunUser>,
}

impl Cmd {
    pub fn new(program: HostPath) -> Cmd {
        Cmd {
            program,
            args: Vec::new(),
            env: Vec::new(),
            stdin: Vec::new(),
            user: None,
        }
    }

    pub fn user(mut self, user: RunUser) -> Cmd {
        self.user = Some(user);
        self
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Cmd {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Cmd {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn stdin(mut self, bytes: Vec<u8>) -> Cmd {
        self.stdin = bytes;
        self
    }
}

/// `(as <user>) <program> <args>`, then the size of the input and the names (never the
/// values) of the environment variables.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "(as {}) ", user.name)?;
        }
        write!(f, "{}", self.program)?;
        for arg in &self.args {
            write!(f, " {}", Quoted(arg))?;
        }
        if !self.stdin.is_empty() {
            write!(f, " (stdin {} bytes)", self.stdin.len())?;
        }
        let mut keys = self.env.iter().map(|(k, _)| k.as_str());
        if let Some(first) = keys.next() {
            write!(f, " (env {first}")?;
            for key in keys {
                write!(f, ", {key}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

/// What a [`Step::Capture`] makes of a command's output before writing it to a file.
/// The output may hold a secret, so the function's error never quotes it and the step
/// prints only `name`. Two filters are equal when their names are.
#[derive(Clone, Copy, Debug)]
pub struct Filter {
    pub name: &'static str,
    pub apply: fn(&[u8]) -> Result<Vec<u8>, String>,
}

impl PartialEq for Filter {
    fn eq(&self, other: &Filter) -> bool {
        self.name == other.name
    }
}

impl Eq for Filter {}

/// A plain-HTTP URL a step waits on, such as a readiness endpoint on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    url: String,
    authority: String,
    host: String,
    port: u16,
    path: String,
}

/// Why a string is not an [`HttpUrl`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("{0:?} is not an http:// URL (no TLS yet)")]
    Scheme(String),
    #[error("{0:?} names no host")]
    NoHost(String),
    #[error("{0:?} has a port that is not a number from 1 to 65535")]
    Port(String),
}

impl HttpUrl {
    pub fn new(url: &str) -> Result<HttpUrl, UrlError> {
        let Some(rest) = url.strip_prefix("http://") else {
            return Err(UrlError::Scheme(url.to_owned()));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // `[::1]:9000`: the port follows the bracket; `host:9000`: the last colon.
        let split = match authority.rfind(']') {
            Some(close) => authority[close..].find(':').map(|i| close + i),
            None => authority.rfind(':'),
        };
        let (host, port) = match split {
            Some(i) => {
                let port = authority[i + 1..].parse::<u16>().ok().filter(|p| *p != 0);
                let Some(port) = port else {
                    return Err(UrlError::Port(url.to_owned()));
                };
                (&authority[..i], port)
            }
            None => (authority, 80),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return Err(UrlError::NoHost(url.to_owned()));
        }
        Ok(HttpUrl {
            url: url.to_owned(),
            authority: authority.to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    /// The host and port as the URL writes them (`[::1]:9000`), for the `Host` header.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path and query sent in the request line, `/` when the URL has none.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

/// One change to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Replace the file at `path` with `bytes`, atomically; `owner: None` leaves the
    /// owner to whoever runs the step.
    WriteFile {
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
        bytes: Vec<u8>,
    },
    /// Make the directory at `path` (its parent must exist), or set its permissions and
    /// owner when it exists.
    Mkdir {
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
    },
    /// Point the symbolic link at `path` to `target` (relative to the link's directory
    /// unless absolute), atomically: a new link is made beside it and renamed over it, so
    /// `path` names the old target or the new one at every moment. A file or directory at
    /// `path` is refused, never replaced; its parent must exist.
    Symlink { path: HostPath, target: PathBuf },
    /// Remove the file, link or empty directory at `path`.
    Remove { path: HostPath },
    /// Run an external tool; a non-zero exit fails the step.
    Run(Cmd),
    /// Run an external tool and write what `filter` makes of its standard output to
    /// `path`, atomically, as [`Step::WriteFile`] does; a non-zero exit or a refusal by
    /// the filter fails the step and writes nothing. The output is never printed.
    Capture {
        cmd: Cmd,
        filter: Filter,
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
    },
    /// Wait until `url` answers a 2xx status, for at most `deadline`.
    WaitHttp { url: HttpUrl, deadline: Duration },
}

/// The line a step prints. It never shows file contents or environment values, which
/// may be secrets.
impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            } => {
                write!(
                    f,
                    "write {path} ({perms}{}, {} bytes)",
                    Owned(owner),
                    bytes.len()
                )
            }
            Step::Mkdir { path, perms, owner } => {
                write!(f, "mkdir {path} ({perms}{})", Owned(owner))
            }
            Step::Symlink { path, target } => {
                write!(f, "link {path} -> {}", Quoted(&target.to_string_lossy()))
            }
            Step::Remove { path } => write!(f, "remove {path}"),
            Step::Run(cmd) => write!(f, "run {cmd}"),
            Step::Capture {
                cmd,
                filter,
                path,
                perms,
                owner,
            } => write!(
                f,
                "run {cmd} > {path} ({perms}{}, through {})",
                Owned(owner),
                filter.name
            ),
            Step::WaitHttp { url, deadline } => {
                write!(f, "wait up to {}s for {url}", deadline.as_secs())
            }
        }
    }
}

struct Owned<'a>(&'a Option<Owner>);

impl fmt::Display for Owned<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(o) => write!(f, ", owner {}:{}", o.uid, o.gid),
            None => Ok(()),
        }
    }
}

/// An argument as one word: quoted when it is empty or holds a space, a quote or a
/// backslash, so a printed line shows where each argument ends.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plain = !self.0.is_empty()
            && !self
                .0
                .chars()
                .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '\\');
        if plain {
            f.write_str(self.0)
        } else {
            write!(f, "{:?}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> HostPath {
        HostPath::new(s).expect("valid test path")
    }

    fn copy(b: &[u8]) -> Result<Vec<u8>, String> {
        Ok(b.to_vec())
    }

    /// Catches a printed line that shows a secret (file bytes or an environment value),
    /// one that hides where an argument ends, and one that hides which user a command
    /// runs as; these lines are what an operator reads before `--apply`.
    #[test]
    fn each_step_prints_one_line_without_secrets() {
        let root = Some(Owner { uid: 0, gid: 0 });
        let cases = [
            (
                Step::WriteFile {
                    path: p("/etc/kbf/scep.env"),
                    perms: Perms::new(0o600),
                    owner: root,
                    bytes: b"KEY=secret\n".to_vec(),
                },
                "write /etc/kbf/scep.env (0600, owner 0:0, 11 bytes)",
            ),
            (
                Step::WriteFile {
                    path: p("/srv/kbf/a"),
                    perms: Perms::new(0o644),
                    owner: None,
                    bytes: Vec::new(),
                },
                "write /srv/kbf/a (0644, 0 bytes)",
            ),
            (
                Step::Mkdir {
                    path: p("/etc/kbf"),
                    perms: Perms::new(0o755),
                    owner: Some(Owner { uid: 0, gid: 20 }),
                },
                "mkdir /etc/kbf (0755, owner 0:20)",
            ),
            (
                Step::Mkdir {
                    path: p("/srv/x"),
                    perms: Perms::new(0o1777),
                    owner: None,
                },
                "mkdir /srv/x (1777)",
            ),
            (
                Step::Symlink {
                    path: p("/usr/local/kbf/current"),
                    target: PathBuf::from("releases/0123"),
                },
                "link /usr/local/kbf/current -> releases/0123",
            ),
            (
                Step::Symlink {
                    path: p("/srv/a"),
                    target: PathBuf::from("/srv/b c"),
                },
                r#"link /srv/a -> "/srv/b c""#,
            ),
            (Step::Remove { path: p("/etc/x") }, "remove /etc/x"),
            (
                Step::Run(Cmd::new(p("/usr/bin/systemctl")).arg("daemon-reload")),
                "run /usr/bin/systemctl daemon-reload",
            ),
            (
                Step::Run(
                    Cmd::new(p("/usr/bin/mc"))
                        .arg("mb")
                        .arg("a b")
                        .arg("")
                        .arg("it's")
                        .arg("q\"")
                        .arg("back\\slash")
                        .env("MC_HOST_x", "http://key:secret@127.0.0.1:9000")
                        .env("HOME", "/root"),
                ),
                r#"run /usr/bin/mc mb "a b" "" "it's" "q\"" "back\\slash" (env MC_HOST_x, HOME)"#,
            ),
            (
                Step::Run(
                    Cmd::new(p("/usr/bin/podman"))
                        .arg("pull")
                        .user(RunUser {
                            name: "kbf".to_owned(),
                            uid: 990,
                            gid: 990,
                            groups: vec![990, 44],
                            home: "/var/lib/kbf".to_owned(),
                        })
                        .env("HOME", "/var/lib/kbf"),
                ),
                "run (as kbf) /usr/bin/podman pull (env HOME)",
            ),
            (
                Step::Run(
                    Cmd::new(p("/usr/bin/kubectl"))
                        .arg("apply")
                        .stdin(b"{\"secret\":1}".to_vec()),
                ),
                "run /usr/bin/kubectl apply (stdin 12 bytes)",
            ),
            (
                Step::Capture {
                    cmd: Cmd::new(p("/usr/bin/mc"))
                        .arg("add")
                        .stdin(b"{}".to_vec())
                        .env("MC_HOST_x", "http://key:secret@127.0.0.1:9000"),
                    filter: Filter {
                        name: "s3-env",
                        apply: copy,
                    },
                    path: p("/etc/kbf/s3.env"),
                    perms: Perms::new(0o600),
                    owner: root,
                },
                "run /usr/bin/mc add (stdin 2 bytes) (env MC_HOST_x) > /etc/kbf/s3.env \
                 (0600, owner 0:0, through s3-env)",
            ),
            (
                Step::WaitHttp {
                    url: HttpUrl::new("http://127.0.0.1:9000/minio/health/ready").expect("url"),
                    deadline: Duration::from_secs(30),
                },
                "wait up to 30s for http://127.0.0.1:9000/minio/health/ready",
            ),
        ];
        for (step, want) in cases {
            assert_eq!(step.to_string(), want);
        }
    }

    /// Catches a URL parser that takes a TLS URL it cannot speak, loses the port of a
    /// bracketed IPv6 host, or accepts port 0 or a host-less URL.
    #[test]
    fn an_http_url_names_host_port_and_path() {
        let ok = [
            (
                "http://127.0.0.1:9000/readyz",
                "127.0.0.1:9000",
                "127.0.0.1",
                9000,
                "/readyz",
            ),
            ("http://localhost", "localhost", "localhost", 80, "/"),
            (
                "http://[::1]:8982/a?b=c",
                "[::1]:8982",
                "::1",
                8982,
                "/a?b=c",
            ),
            ("http://[::1]/", "[::1]", "::1", 80, "/"),
        ];
        for (url, authority, host, port, path) in ok {
            let u = HttpUrl::new(url).expect(url);
            let got = (u.authority(), u.host(), u.port(), u.path());
            assert_eq!(got, (authority, host, port, path), "{url}");
            assert_eq!(u.to_string(), url);
        }
        type Variant = fn(String) -> UrlError;
        let refused: [(&str, Variant); 6] = [
            ("https://h/", UrlError::Scheme),
            ("h:80/", UrlError::Scheme),
            ("http://:80/", UrlError::NoHost),
            ("http:///x", UrlError::NoHost),
            ("http://h:0/", UrlError::Port),
            ("http://h:x/", UrlError::Port),
        ];
        for (url, want) in refused {
            assert_eq!(HttpUrl::new(url), Err(want(url.to_owned())), "{url}");
        }
    }

    /// Catches filters compared by function address (two copies of one kit's filter
    /// would then differ) instead of by the name the step prints.
    #[test]
    fn filters_are_equal_by_name() {
        let a = Filter {
            name: "a",
            apply: copy,
        };
        let b = Filter { name: "b", ..a };
        assert_eq!(a, Filter { ..a });
        assert_ne!(a, b);
        assert_eq!((a.apply)(b"x"), Ok(b"x".to_vec()));
    }

    #[test]
    fn perms_keep_their_bits() {
        assert_eq!(Perms::new(0o4755).bits(), 0o4755);
    }

    /// Catches a constructor that masks bits above 0o7777 instead of refusing them.
    #[test]
    #[should_panic(expected = "at most 0o7777")]
    fn perms_above_0o7777_are_refused() {
        let bits = std::hint::black_box(0o10000);
        let _ = Perms::new(bits);
    }
}
