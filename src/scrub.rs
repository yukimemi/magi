//! Last-line scrub for text that lands on GitHub.
//!
//! The prompts tell agents not to put machine- or operator-identifying data
//! (hostnames, account names, IPs, home paths, emails, tokens) into pull
//! requests and issues, but a prompt is advisory. [`scrub`] is the enforced
//! half: shared rules used by the GitHub text posting gate.
//!
//! It scans the input left to right exactly once and pushes into a *separate*
//! output string, so a replacement is never scanned again (compare the
//! `blind::redact` loop, whose `[REDACTED]` contains the letters of the words it
//! hunted). It prefers missing something to mangling ordinary prose: repo-
//! relative paths, URLs, `std::io::Error`, `v1.2.3` and `@handle` all pass.
//! Dotted IPv4-shaped section references after `§`, `section`, `sec.` or
//! `clause` also pass; the exception never applies to IPv6.
//! A few conventional public system paths also pass the absolute-path rule;
//! configured homes and known identity words within them are still scrubbed.

/// The local facts worth hiding. `Default` is "nothing known", which leaves only
/// the pattern-based rules.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// Local account name.
    pub user: String,
    /// Machine hostname.
    pub host: String,
    /// Home directory path.
    pub home: String,
}

impl Identity {
    /// Read the current account, host and home directory. The only I/O here.
    pub fn current() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let host = env("HOSTNAME")
            .or_else(|| env("COMPUTERNAME"))
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
        Self {
            user: env("USER").or_else(|| env("USERNAME")).unwrap_or_default(),
            host,
            home: dirs::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

const TOKEN_PREFIXES: [&str; 9] = [
    "AKIA",
    "github_pat_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "sk-",
    "xoxb-",
    "xoxp-",
];
const TOKEN_MIN_TAIL: usize = 16;
/// Identity words shorter than this are too likely to be ordinary prose.
const MIN_IDENTITY_WORD: usize = 3;

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_name(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

/// A path component: the name run minus trailing dots, which are punctuation.
fn name_len(s: &str) -> usize {
    s[..run(s, is_name)].trim_end_matches('.').len()
}

/// Length in bytes of the leading run of `s` whose chars satisfy `f`.
fn run(s: &str, f: impl Fn(char) -> bool) -> usize {
    s.char_indices()
        .find(|&(_, c)| !f(c))
        .map_or(s.len(), |(i, _)| i)
}

/// Replace machine- and operator-identifying data in `text`. Pure.
pub fn scrub(text: &str, id: &Identity) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((len, rep)) = match_at(&text[..i], &text[i..], id) {
            out.push_str(rep);
            i += len;
        } else {
            let c = text[i..].chars().next().expect("i is on a char boundary");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Where [`scrub`] would replace something: `(line, kind)` per match, lines
/// counted from 1. Never carries the matched text. Pure.
pub fn locate(text: &str, id: &Identity) -> Vec<(usize, &'static str)> {
    let mut hits = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < text.len() {
        if let Some((len, rep)) = match_at(&text[..i], &text[i..], id) {
            let kind = match rep {
                "~" => "home-path",
                r => r
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim_start_matches("redacted-"),
            };
            hits.push((line, kind));
            line += text[i..i + len].matches('\n').count();
            i += len;
        } else {
            let c = text[i..].chars().next().expect("i is on a char boundary");
            if c == '\n' {
                line += 1;
            }
            i += c.len_utf8();
        }
    }
    hits
}

fn match_at(before: &str, rest: &str, id: &Identity) -> Option<(usize, &'static str)> {
    let prev = before.chars().next_back();
    let starts_word = prev.is_none_or(|p| !is_word(p));
    home_path(rest, prev, id)
        .or_else(|| {
            starts_word
                .then(|| {
                    token(rest).or_else(|| credential(rest)).or_else(|| {
                        prev.is_none_or(|c| !is_name(c))
                            .then(|| named_identity(rest, prev))
                            .flatten()
                    })
                })
                .flatten()
        })
        .or_else(|| absolute_path(rest, prev))
        .or_else(|| {
            prev.is_none_or(|p| !is_email_local(p))
                .then(|| email(rest))
                .flatten()
        })
        .or_else(|| {
            prev.is_none_or(|p| !p.is_ascii_alphanumeric() && p != '.' && p != ':')
                .then(|| {
                    ipv4(rest)
                        .filter(|_| !section_reference(before))
                        .or_else(|| ipv6(rest))
                })
                .flatten()
        })
        .or_else(|| {
            (starts_word && !github_owner_slot(before))
                .then(|| identity_word(rest, id))
                .flatten()
        })
}

/// `/Users/x`, `/home/x`, `/root`, `C:\Users\x`, `C:/Users/x` and the literal
/// home directory, each collapsed to `~` so the repo-relative tail survives.
fn home_path(rest: &str, prev: Option<char>, id: &Identity) -> Option<(usize, &'static str)> {
    if prev.is_some_and(|p| p.is_ascii_alphanumeric() || matches!(p, '.' | '_' | '-' | '~')) {
        return None;
    }
    if id.home.len() > 1 && rest.starts_with(id.home.as_str()) {
        let tail = &rest[id.home.len()..];
        if tail.chars().next().is_none_or(|c| !is_name(c)) {
            return Some((id.home.len(), "~"));
        }
    }
    for base in ["/Users/", "/home/"] {
        if let Some(tail) = rest.strip_prefix(base) {
            let n = name_len(tail);
            if n > 0 {
                return Some((base.len() + n, "~"));
            }
        }
    }
    if let Some(tail) = rest.strip_prefix("/root")
        && tail.chars().next().is_none_or(|c| !is_word(c))
    {
        return Some(("/root".len(), "~"));
    }
    let b = rest.as_bytes();
    if b.len() > 9
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && matches!(b[2], b'\\' | b'/')
        && b[3..8].eq_ignore_ascii_case(b"users")
        && matches!(b[8], b'\\' | b'/')
    {
        let n = name_len(&rest[9..]);
        if n > 0 {
            return Some((9 + n, "~"));
        }
    }
    None
}

fn token(rest: &str) -> Option<(usize, &'static str)> {
    let prefix = TOKEN_PREFIXES.iter().find(|p| rest.starts_with(**p))?;
    let tail = run(&rest[prefix.len()..], is_word);
    (tail >= TOKEN_MIN_TAIL).then_some((prefix.len() + tail, "[redacted-token]"))
}

/// Explicit assignments avoid guessing whether an ordinary word is an account.
fn named_identity(rest: &str, prev: Option<char>) -> Option<(usize, &'static str)> {
    for key in [
        "hostname=",
        "hostname: ",
        "username=",
        "username: ",
        "user=",
        "host=",
    ] {
        if rest
            .get(..key.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(key))
        {
            let n = run(&rest[key.len()..], is_name);
            if n > 0 {
                return Some((key.len() + n, "[redacted-identity]"));
            }
        }
    }
    let n = name_len(rest);
    if n > 0
        && !code_receiver(&rest[..n], prev == Some('`') && rest[n..].starts_with('`'))
        && [".local", ".internal", ".lan"].iter().any(|suffix| {
            n.checked_sub(suffix.len())
                .and_then(|start| rest.get(start..n))
                .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
        })
    {
        return Some((n, "[redacted-host]"));
    }
    None
}

/// A dotted name that reads as a code path: one dot, no hyphen, and either
/// a `self.` / `this.` receiver, a CamelCase one, or a lowercase ASCII
/// identifier containing an underscore or directly enclosed in backticks.
/// Only the receiver is inspected, so the suffix's case never matters.
/// Unknown CamelCase hosts and backticked alphabetic hosts also read as code;
/// the operator's known hostname is still caught by `identity_word`.
fn code_receiver(name: &str, backticked: bool) -> bool {
    if name.matches('.').count() != 1 || name.contains('-') {
        return false;
    }
    let Some((receiver, _)) = name.split_once('.') else {
        return false;
    };
    if receiver.eq_ignore_ascii_case("self") || receiver.eq_ignore_ascii_case("this") {
        return true;
    }
    let lowercase_identifier =
        !receiver.is_empty() && receiver.chars().all(|c| c.is_ascii_lowercase() || c == '_');
    if lowercase_identifier && (backticked || receiver.contains('_')) {
        return true;
    }
    let mut prev_lower = false;
    receiver.chars().any(|c| {
        let hump = prev_lower && c.is_ascii_uppercase();
        prev_lower = c.is_ascii_lowercase();
        hump
    })
}

/// `"password": "value"` and `'token':'value'`: the key is quoted, so the
/// unquoted `key=` / `key: ` forms below never match.
fn quoted_credential(rest: &str) -> Option<(usize, &'static str)> {
    for key in [
        "password", "token", "api_key", "api-key", "apikey", "secret",
    ] {
        let Some(after) = rest
            .get(..key.len())
            .filter(|p| p.eq_ignore_ascii_case(key))
            .map(|_| &rest[key.len()..])
        else {
            continue;
        };
        let Some(after_quote) = after.strip_prefix(['"', '\'']) else {
            continue;
        };
        let t = after_quote.trim_start();
        let Some(t) = t.strip_prefix([':', '=']) else {
            continue;
        };
        let t = t.trim_start();
        // A quoted value runs to its closing quote and may hold spaces or commas.
        let n = match t.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let inner = &t[1..];
                let end = quoted_end(inner, q).unwrap_or(inner.len());
                1 + end + usize::from(end < inner.len())
            }
            _ => run(t, |c| {
                !c.is_whitespace() && !matches!(c, '`' | '<' | '>' | ',' | '}')
            }),
        };
        let value = t;
        if n > 0 {
            return Some((rest.len() - value.len() + n, "[redacted-token]"));
        }
    }
    None
}

/// Byte offset of the first `q` in `inner` that no backslash escapes.
fn quoted_end(inner: &str, q: char) -> Option<usize> {
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == q {
            return Some(i);
        }
    }
    None
}

fn credential(rest: &str) -> Option<(usize, &'static str)> {
    if let Some(hit) = quoted_credential(rest) {
        return Some(hit);
    }
    for key in [
        "password=",
        "token=",
        "api_key=",
        "api-key=",
        "password: ",
        "token: ",
        "api_key: ",
        "bearer ",
        "password ",
        "token ",
        "api key ",
    ] {
        if rest
            .get(..key.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(key))
        {
            let tail = &rest[key.len()..];
            let padding = tail.len() - tail.trim_start_matches([' ', '\'', '"']).len();
            let value = &tail[padding..];
            let n = run(value, |c| {
                !c.is_whitespace() && !matches!(c, '`' | '<' | '>' | '"' | '\'')
            });
            let contextual = matches!(key, "password " | "token " | "api key ");
            let entropy = n >= 20
                && value[..n].bytes().any(|b| b.is_ascii_digit())
                && value[..n].bytes().any(|b| b.is_ascii_alphabetic());
            if n > 0 && (!contextual || entropy) {
                return Some((key.len() + padding + n, "[redacted-token]"));
            }
        }
    }
    None
}

fn absolute_path(rest: &str, prev: Option<char>) -> Option<(usize, &'static str)> {
    if prev.is_some_and(|c| c.is_alphanumeric() || matches!(c, '/' | ':' | '.' | '~')) {
        return None;
    }
    let b = rest.as_bytes();
    let windows =
        b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    // Known filesystem roots, rather than arbitrary slash-prefixed API routes.
    let unix = [
        "/tmp/",
        "/private/",
        "/var/",
        "/etc/",
        "/opt/",
        "/srv/",
        "/usr/",
        "/mnt/",
        "/Volumes/",
    ]
    .iter()
    .any(|root| rest.starts_with(root));
    if windows || unix {
        let n = run(rest, |c| {
            !c.is_whitespace() && !matches!(c, '`' | '"' | '\'' | '<' | '>')
        });
        if unix && public_system_path(&rest[..n]) {
            return None;
        }
        return Some((n, "[redacted-path]"));
    }
    None
}

fn public_system_path(path: &str) -> bool {
    // Exact public locations, with no variable account/host/checkout/mount slot:
    // /etc/hosts is the standard host table; /usr/local/bin is the command
    // directory and foo a literal example command; /var/log/syslog is the
    // standard system log; /var/run/kanade/agent.sock is the documented service
    // endpoint. Do not generalize to arbitrary command, log or service names:
    // those may identify an operator even when Identity does not know the name.
    // Home rules retain priority and other identity rules still scan these paths.
    // Ignore prose punctuation only for this check, preserving the original
    // redaction boundary. Exact matching rejects traversal and extra levels.
    matches!(
        path.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']),
        "/etc/hosts"
            | "/usr/local/bin"
            | "/usr/local/bin/foo"
            | "/var/log/syslog"
            | "/var/run/kanade/agent.sock"
    )
}

fn email(rest: &str) -> Option<(usize, &'static str)> {
    let local = run(rest, is_email_local);
    if local == 0 || !rest[local..].starts_with('@') {
        return None;
    }
    let domain = &rest[local + 1..];
    let mut end = 0;
    let mut labels = 0;
    loop {
        let n = run(&domain[end..], |c| c.is_ascii_alphanumeric() || c == '-');
        if n == 0 {
            break;
        }
        end += n;
        labels += 1;
        if domain[end..].starts_with('.')
            && run(&domain[end + 1..], |c| c.is_ascii_alphanumeric()) > 0
        {
            end += 1;
        } else {
            break;
        }
    }
    (labels >= 2).then_some((local + 1 + end, "[redacted-email]"))
}

/// Only an immediately preceding marker counts, with no newline between it
/// and the number. English markers require horizontal whitespace; `§` does not.
fn section_reference(before: &str) -> bool {
    let trimmed = before.trim_end_matches([' ', '\t']);
    ["§", "section", "sec.", "clause"].iter().any(|marker| {
        if *marker != "§" && trimmed.len() == before.len() {
            return false;
        }
        let Some(start) = trimmed.len().checked_sub(marker.len()) else {
            return false;
        };
        trimmed
            .get(start..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(marker))
            && trimmed[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-'))
    })
}

fn ipv4(rest: &str) -> Option<(usize, &'static str)> {
    let mut len = 0;
    for part in 0..4 {
        let s = &rest[len..];
        let n = run(s, |c| c.is_ascii_digit());
        if n == 0 || n > 3 || s[..n].parse::<u16>().ok()? > 255 {
            return None;
        }
        len += n;
        if part < 3 {
            if !rest[len..].starts_with('.') {
                return None;
            }
            len += 1;
        }
    }
    let after = &rest[len..];
    let mut chars = after.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => return None,
        Some('.') if chars.next().is_some_and(|c| c.is_ascii_digit()) => return None,
        _ => {}
    }
    Some((len, "[redacted-ip]"))
}

fn ipv6(rest: &str) -> Option<(usize, &'static str)> {
    let mut n = run(rest, |c| c.is_ascii_hexdigit() || c == ':');
    // A trailing single colon is punctuation, not part of the address.
    while n > 0 && rest[..n].ends_with(':') && !rest[..n].ends_with("::") {
        n -= 1;
    }
    let cand = &rest[..n];
    let colons = cand.matches(':').count();
    let shaped = (cand.contains("::") && colons >= 2) || colons == 7;
    if !shaped
        || !cand.bytes().any(|b| b.is_ascii_digit())
        || cand.split(':').any(|g| g.len() > 4)
        || rest[n..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some((n, "[redacted-ip]"))
}

/// Plain words (and the bare mDNS suffix) that show up as a hostname or account
/// name on some machines but are ordinary prose everywhere. Matched against the
/// whole trimmed value only, so `devbox.local` is still a hostname.
const GENERIC_IDENTITY_WORDS: [&str; 12] = [
    "local",
    "localhost",
    "localdomain",
    "lan",
    "internal",
    "user",
    "host",
    "admin",
    "root",
    "test",
    "default",
    "unknown",
];

fn is_generic_identity(w: &str) -> bool {
    let w = w.strip_prefix('.').unwrap_or(w);
    GENERIC_IDENTITY_WORDS
        .iter()
        .any(|g| g.eq_ignore_ascii_case(w))
}

/// True when the text before the cursor ends with `github.com/` as the start of
/// a URL, i.e. the cursor is on the owner segment of a GitHub URL.
fn github_owner_slot(before: &str) -> bool {
    let Some(head) = strip_suffix_ci(before.as_bytes(), b"github.com/") else {
        return false;
    };
    let head = strip_suffix_ci(head, b"www.").unwrap_or(head);
    strip_suffix_ci(head, b"http://").is_some() || strip_suffix_ci(head, b"https://").is_some()
}

fn strip_suffix_ci<'a>(s: &'a [u8], suffix: &[u8]) -> Option<&'a [u8]> {
    let start = s.len().checked_sub(suffix.len())?;
    s[start..]
        .eq_ignore_ascii_case(suffix)
        .then_some(&s[..start])
}

fn identity_word(rest: &str, id: &Identity) -> Option<(usize, &'static str)> {
    for (word, rep) in [(&id.user, "[redacted-user]"), (&id.host, "[redacted-host]")] {
        let w = word.trim();
        if w.len() < MIN_IDENTITY_WORD
            || is_generic_identity(w)
            || rest.len() < w.len()
            || !rest.is_char_boundary(w.len())
        {
            continue;
        }
        if rest[..w.len()].eq_ignore_ascii_case(w)
            && rest[w.len()..].chars().next().is_none_or(|c| !is_word(c))
        {
            return Some((w.len(), rep));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity {
            user: "alice".into(),
            host: "buildbox".into(),
            home: "/srv/people/alice".into(),
        }
    }

    fn s(t: &str) -> String {
        scrub(t, &id())
    }

    #[test]
    fn locate_names_kind_and_line_without_the_text() {
        let id = Identity::default();
        let text = "fine\ntoken=abcdefghijklmnop1234 here\nmail a@b.example";
        assert_eq!(locate(text, &id), vec![(2, "token"), (3, "email")]);
    }

    #[test]
    fn quoted_credentials_honor_escaped_quotes() {
        let out = s(r#"{"password":"prefix\"hunter2"} tail"#);
        assert!(!out.contains("hunter2") && !out.contains("prefix"), "{out}");
        assert!(out.ends_with("} tail"), "{out}");
        // An even run of backslashes leaves the quote as the closer.
        let out = s(r#"{"password":"a\\\\"} tail"#);
        assert!(out.ends_with("} tail") && !out.contains("a\\"), "{out}");
        // An odd run escapes it.
        let out = s(r#"{"password":"a\\\"hunter2"} tail"#);
        assert!(!out.contains("hunter2"), "{out}");
        let out = s(r#"{'token':'x\'hunter2'} tail"#);
        assert!(!out.contains("hunter2"), "{out}");
        let out = s(r#"{"password":"日本\"語hunter2"} tail"#);
        assert!(!out.contains("hunter2") && !out.contains('語'), "{out}");
        // No closing quote: everything after is removed.
        let out = s(r#"{"password":"abc\"hunter2"#);
        assert!(!out.contains("hunter2"), "{out}");
    }

    #[test]
    fn home_paths_collapse_and_keep_the_tail() {
        assert_eq!(s("see /Users/bob/src/x.rs now"), "see ~/src/x.rs now");
        assert_eq!(s("in /home/bob-1/.config"), "in ~/.config");
        assert_eq!(s("at C:\\Users\\Bob\\proj\\a.rs"), "at ~\\proj\\a.rs");
        assert_eq!(s("at C:/Users/Bob/proj"), "at ~/proj");
        assert_eq!(s("/root/x and /root."), "~/x and ~.");
        assert_eq!(s("/srv/people/alice/wt/a"), "~/wt/a");
    }

    #[test]
    fn public_system_paths_pass_scrub_and_locate() {
        for path in [
            "/run/kanade/agent.sock",
            "/var/run/kanade/agent.sock",
            "/etc/hosts",
            "/usr/local/bin",
            "/usr/local/bin/foo",
            "/var/log/syslog",
        ] {
            for text in [
                path.to_owned(),
                format!("uses `{path}` on this platform"),
                format!("uses \"{path}\" on this platform"),
                format!("See ({path}), then {path}."),
            ] {
                assert_eq!(s(&text), text, "{text}");
                assert!(locate(&text, &id()).is_empty(), "{text}");
            }
        }
    }

    #[test]
    fn private_paths_and_non_public_shapes_still_redact() {
        let identity = Identity::default();
        for path in [
            "/Volumes/disk-name/project",
            "/Volumes/disk name/project",
            "/srv/people/alice/wt/a",
            "/tmp/checkout/build",
            "/private/var/folders/ab/account/build",
            "/var/folders/ab/account/build",
            "/mnt/disk/project",
            "/usr/home/bob/project",
            "/opt/checkout/build",
            "/etc/hosts/private",
            "/etc/hostname",
            "/etc/hosts-backup",
            "/usr/local/bin/foo/private",
            "/usr/local/bin/../private",
            "/usr/local/bin/..",
            "/usr/local/bin/./foo",
            "/usr/local/bin//foo",
            "/usr/local/bin/foo/",
            "/var/log/../private",
            "/var/log/service/private",
            "/var/run/agent.sock",
            "/var/run/service/agent.pid",
            "/var/run/service/.sock",
            "/var/run/../agent.sock",
            "/var/run/service/../agent.sock",
            "/var/run//agent.sock",
            r"/var/log/foo\private",
            r"C:\build\checkout",
        ] {
            assert!(
                scrub(path, &identity).starts_with("[redacted-path]"),
                "{path}"
            );
            assert_eq!(locate(path, &identity), vec![(1, "path")], "{path}");
        }
        for (text, expected) in [
            ("/Users/bob/src/x", "~/src/x"),
            ("/home/bob/.config", "~/.config"),
            ("C:/Users/Bob/proj", "~/proj"),
            (r"C:\Users\Bob\proj", r"~\proj"),
            ("/srv/people/alice/wt/a", "~/wt/a"),
        ] {
            assert_eq!(s(text), expected, "{text}");
            assert_eq!(locate(text, &id()), vec![(1, "home-path")], "{text}");
        }
        // The existing whitespace consumption boundary stays unchanged.
        assert_eq!(
            scrub("/Volumes/disk name/project", &identity),
            "[redacted-path] name/project"
        );
        for text in [
            "/unknown/checkout/build",
            "/api/v1/runs",
            "/run/service/agent.sock",
        ] {
            assert_eq!(scrub(text, &identity), text);
            assert!(locate(text, &identity).is_empty());
        }
    }

    #[test]
    fn public_paths_keep_home_and_identity_rules_and_do_not_rescan() {
        let identity = Identity {
            home: "/usr/local/bin".into(),
            ..id()
        };
        assert_eq!(scrub("/usr/local/bin/foo", &identity), "~/foo");
        assert_eq!(
            locate("/usr/local/bin/foo", &identity),
            vec![(1, "home-path")]
        );
        for path in [
            "/usr/local/bin/alice",
            "/usr/local/bin/alice-build",
            "/usr/local/bin/bob",
            "/var/log/buildbox.log",
            "/var/log/alice-build.log",
            "/var/run/alice/agent.sock",
            "/var/run/alice-agent/agent.sock",
            "/var/run/bob/agent.sock",
            "/var/log/box.local",
        ] {
            for identity in [id(), Identity::default()] {
                assert_eq!(scrub(path, &identity), "[redacted-path]", "{path}");
                assert_eq!(locate(path, &identity), vec![(1, "path")], "{path}");
            }
        }
        let identity = Identity {
            user: "foo".into(),
            host: "kanade".into(),
            ..Identity::default()
        };
        for (text, expected, kind) in [
            (
                "/usr/local/bin/foo",
                "/usr/local/bin/[redacted-user]",
                "user",
            ),
            (
                "/var/run/kanade/agent.sock",
                "/var/run/[redacted-host]/agent.sock",
                "host",
            ),
        ] {
            assert_eq!(scrub(text, &identity), expected);
            assert_eq!(locate(text, &identity), vec![(1, kind)]);
        }
        let identity = Identity {
            user: "redacted".into(),
            host: "redacted-path".into(),
            ..Identity::default()
        };
        let text = "/tmp/checkout redacted /usr/local/bin/foo";
        assert_eq!(
            scrub(text, &identity),
            "[redacted-path] [redacted-user] /usr/local/bin/foo"
        );
        assert_eq!(locate(text, &identity), vec![(1, "path"), (1, "user")]);
    }

    #[test]
    fn emails_ips_and_tokens() {
        assert_eq!(s("mail dev.x+y@example.co.uk!"), "mail [redacted-email]!");
        assert_eq!(s("host 192.168.0.12:8080"), "host [redacted-ip]:8080");
        assert_eq!(
            s("v6 fe80::1 and ::1"),
            "v6 [redacted-ip] and [redacted-ip]"
        );
        assert_eq!(s("full 2001:db8:0:0:0:0:0:1."), "full [redacted-ip].");
        assert_eq!(
            s("tok ghp_abcdefghijklmnopqrstuv end"),
            "tok [redacted-token] end"
        );
    }

    #[test]
    fn ipv4_section_references_pass_scrub_and_locate() {
        for text in [
            "§2.12.4.1-4.4",
            "see §2.12.4.1",
            "§ 2.12.4.1",
            "section 2.12.4.1",
            "sec. 2.12.4.1",
            "clause 2.12.4.1",
            "SECTION\t2.12.4.1",
            "See (Sec. \t2.12.4.1)",
            "CLAUSE 2.12.4.1",
            "仕様 §\t2.12.4.1",
            "released v1.2.3, version 0.41.1, 300.1.1.1",
        ] {
            assert_eq!(s(text), text, "{text}");
            assert!(locate(text, &id()).is_empty(), "{text}");
        }
    }

    #[test]
    fn ipv4_section_markers_require_boundaries_and_same_line() {
        for before in [
            "subsection ",
            "xsec. ",
            "9clause ",
            "_section ",
            "-clause ",
            "ésection ",
            "字sec. ",
            "x§ ",
            "section\n",
            "§\n ",
            "clause\r\n\t",
            "section\u{a0}",
            "RFC ",
        ] {
            let text = format!("{before}2.12.4.1");
            assert_eq!(s(&text), format!("{before}[redacted-ip]"), "{text}");
            let line = 1 + before.matches('\n').count();
            assert_eq!(locate(&text, &id()), vec![(line, "ip")], "{text}");
        }
        for before in ["section", "sec.", "clause", "é", "字"] {
            assert!(!section_reference(before), "{before}");
        }
    }

    #[test]
    fn section_exception_keeps_other_sensitive_data_redacted() {
        for (text, expected, hits) in [
            (
                "host 192.168.0.12:8080",
                "host [redacted-ip]:8080",
                vec![(1, "ip")],
            ),
            (
                "/Users/bob 10.0.0.1 a@b.io alice",
                "~ [redacted-ip] [redacted-email] [redacted-user]",
                vec![(1, "home-path"), (1, "ip"), (1, "email"), (1, "user")],
            ),
            (
                "connect to 10.1.2.3",
                "connect to [redacted-ip]",
                vec![(1, "ip")],
            ),
            (
                "§2.12.4.1\nconnect to 10.1.2.3",
                "§2.12.4.1\nconnect to [redacted-ip]",
                vec![(2, "ip")],
            ),
            (
                "section 2.12.4.1-10.1.2.3, 10.0.0.1",
                "section 2.12.4.1-[redacted-ip], [redacted-ip]",
                vec![(1, "ip"), (1, "ip")],
            ),
            (
                "§fe80::1\nsection ::1\nclause 2001:db8:0:0:0:0:0:1",
                "§[redacted-ip]\nsection [redacted-ip]\nclause [redacted-ip]",
                vec![(1, "ip"), (2, "ip"), (3, "ip")],
            ),
        ] {
            assert_eq!(s(text), expected, "{text}");
            assert_eq!(locate(text, &id()), hits, "{text}");
        }
    }

    #[test]
    fn section_context_uses_original_input_and_does_not_rescan_replacements() {
        let identity = Identity {
            user: "redacted-ip".into(),
            ..Identity::default()
        };
        let text = "§2.12.4.1\nconnect to 10.1.2.3";
        assert_eq!(
            scrub(text, &identity),
            "§2.12.4.1\nconnect to [redacted-ip]"
        );
        assert_eq!(locate(text, &identity), vec![(2, "ip")]);
        let identity = Identity {
            user: "section".into(),
            ..Identity::default()
        };
        assert_eq!(
            scrub("section 2.12.4.1", &identity),
            "[redacted-user] 2.12.4.1"
        );
        assert_eq!(locate("section 2.12.4.1", &identity), vec![(1, "user")]);
    }

    #[test]
    fn identity_words_match_whole_words_only() {
        assert_eq!(
            s("by Alice on buildbox"),
            "by [redacted-user] on [redacted-host]"
        );
        assert_eq!(
            s("alicein wonderland, xbuildbox"),
            "alicein wonderland, xbuildbox"
        );
    }

    #[test]
    fn github_url_owner_is_kept_but_other_mentions_are_not() {
        assert_eq!(
            s("https://github.com/alice/magi/pull/681 by alice"),
            "https://github.com/alice/magi/pull/681 by [redacted-user]"
        );
        assert_eq!(
            s("http://www.GitHub.com/Alice/x"),
            "http://www.GitHub.com/Alice/x"
        );
        for t in [
            "https://github.com.evil/alice",
            "https://github.com/x/alice",
            "git@github.com:alice/x",
            "https://api.github.com/alice",
            "github.com/alice",
        ] {
            assert!(s(t).contains("[redacted-user]"), "{t}");
        }
        let text = "https://github.com/alice/magi alice";
        assert_eq!(locate(text, &id()), vec![(1, "user")]);
    }

    #[test]
    fn github_owner_slot_checks_only_ascii_url_suffixes() {
        for before in [
            "https://github.com/",
            "http://github.com/",
            "https://www.github.com/",
            "http://www.github.com/",
            "HTTPS://GitHub.com/",
            "HTTP://WWW.GitHub.com/",
            "prose éhttps://github.com/",
        ] {
            assert!(github_owner_slot(before), "{before}");
        }
        for before in [
            "https://api.github.com/",
            "github.com/",
            "www.github.com/",
            "",
            "git",
            "égithub.com/",
            "éwww.github.com/",
            "é",
        ] {
            assert!(!github_owner_slot(before), "{before}");
        }
    }

    #[test]
    fn long_body_of_short_words_passes_scrub_and_locate() {
        let text = "a ".repeat(32768);
        let id = Identity::default();
        assert_eq!(scrub(&text, &id), text);
        assert!(locate(&text, &id).is_empty());
    }

    #[test]
    fn generic_host_and_user_values_do_not_redact_prose() {
        for host in ["local", ".local", "devbox.local"] {
            let i = Identity {
                host: host.into(),
                ..Identity::default()
            };
            let t = "`WatchState::local` and local mode";
            assert_eq!(scrub(t, &i), t, "{host}");
        }
        let real = Identity {
            host: "devbox.local".into(),
            ..Identity::default()
        };
        assert_eq!(
            scrub("on devbox.local now", &real),
            "on [redacted-host] now"
        );
        for (user, host) in [("local", ""), ("admin", "localhost")] {
            let i = Identity {
                user: user.into(),
                host: host.into(),
                ..Identity::default()
            };
            let t = "local admin localhost";
            assert_eq!(scrub(t, &i), t);
        }
        let once = s("https://github.com/alice/x alice buildbox");
        assert_eq!(scrub(&once, &id()), once);
        assert_eq!(s("on buildbox"), "on [redacted-host]");
    }

    #[test]
    fn dotted_code_paths_ending_in_local_are_not_hosts() {
        for t in [
            "`WatchState.local` is true",
            "WatchState.lan and self.local",
            "WatchState.local",
        ] {
            assert_eq!(scrub(t, &Identity::default()), t);
        }
        // Known trade-off: a CamelCase single-label host reads as code.
        for t in ["ssh MyServer.lan", "JohnsMacBook.local"] {
            assert_eq!(scrub(t, &Identity::default()), t);
        }
        for (t, want) in [
            ("ssh m1air.loCal now", "ssh [redacted-host] now"),
            ("ssh devbox.LOCAL", "ssh [redacted-host]"),
            ("ssh box.InterNal", "ssh [redacted-host]"),
            ("ssh box.LaN", "ssh [redacted-host]"),
        ] {
            assert_eq!(scrub(t, &Identity::default()), want, "{t}");
        }
        for t in [
            "ssh devbox.local",
            "ssh BUILD-SERVER.local",
            "ssh Alices-MacBook-Pro.local",
            "ssh BUILD.local",
        ] {
            assert!(
                scrub(t, &Identity::default()).contains("[redacted-host]"),
                "{t}"
            );
        }
    }

    #[test]
    fn lowercase_field_receivers_pass_scrub_and_locate() {
        let id = Identity::default();
        for text in [
            "`st.local` is true",
            "`cfg.local`",
            "`ctx.lan`",
            "`env.internal`",
            "my_state.local",
            "`my_state.local`",
            "`st.local`, then continue.",
            "`devbox.local`",
        ] {
            assert_eq!(scrub(text, &id), text, "{text}");
            assert!(locate(text, &id).is_empty(), "{text}");
        }
    }

    #[test]
    fn field_receiver_exception_keeps_host_redaction() {
        let id = Identity::default();
        for text in [
            "st.local",
            "cfg.local",
            "devbox.local",
            "box.lan",
            "`st.local",
            "st.local`",
            "`st.local is true`",
            "`st.local.`",
            "`box1.local`",
            "`build-box.local`",
            "`box.example.local`",
        ] {
            assert!(scrub(text, &id).contains("[redacted-host]"), "{text}");
            assert_eq!(locate(text, &id), vec![(1, "host")], "{text}");
        }
        for text in ["host=st.local", "`host=cfg.local`"] {
            assert!(scrub(text, &id).contains("[redacted-identity]"), "{text}");
            assert_eq!(locate(text, &id), vec![(1, "identity")], "{text}");
        }
        let known = Identity {
            host: "devbox.local".into(),
            ..Identity::default()
        };
        let text = "`devbox.local`";
        assert_eq!(scrub(text, &known), "`[redacted-host]`");
        assert_eq!(locate(text, &known), vec![(1, "host")]);
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for t in [
            "Change src/graph.rs and tests/common/mod.rs.",
            "See https://github.com/o/r/pull/12 for #12",
            "returns std::io::Error, or a::b, Vec::new()",
            "released v1.2.3, version 0.41.1, 300.1.1.1",
            "thanks @coderabbitai; at 12:34:56 it ran",
            "/api/v1/runs; docs/home/x and ./Users/y",
            "A plain sentence about background and motivation.",
            "日本語のテキスト 🎉 with émoji",
        ] {
            assert_eq!(s(t), t);
        }
    }

    #[test]
    fn multibyte_input_does_not_panic_and_replacement_is_not_rescanned() {
        assert_eq!(
            s("C:\\日本語 and C:/日本"),
            "[redacted-path] and [redacted-path]"
        );
        assert_eq!(s("é/Users/bob/é 日本 alice"), "é~/é 日本 [redacted-user]");
        let once = s("/Users/bob 10.0.0.1 a@b.io alice");
        assert_eq!(scrub(&once, &Identity::default()), once);
        // A user named like the replacement text cannot loop or re-match.
        let odd = Identity {
            user: "redacted".into(),
            ..Identity::default()
        };
        assert_eq!(scrub("redacted x", &odd), "[redacted-user] x");
    }
}
