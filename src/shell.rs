//! Just enough shell syntax to find commands: words, segments, heredocs.
//!
//! This is not a shell parser. It splits a command line into the parts that each start a
//! program (`a && b | c; d`), skips heredoc and here-string bodies (data, never commands),
//! and splits a part into words. It serves detection and the agent hook only; commands are
//! always run by the real shell.

use std::ops::Range;

/// True when the argv is one shell string ("npm ci && npm test") rather than separate words.
pub fn is_shell_string(argv: &[String]) -> bool {
    argv.len() == 1 && argv[0].trim().contains(char::is_whitespace)
}

/// Split shell words. Quotes group words; backslash escapes only `\"` inside double quotes
/// and `\ ` outside quotes, so Windows paths keep their backslashes.
pub fn tokenize(s: &str) -> Vec<String> {
    words(s).into_iter().map(|w| w.text).collect()
}

/// A word and the byte offset where it starts in the input.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: usize,
}

/// [`tokenize`] with each word's start offset, so callers can edit the original text.
pub fn words(s: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = chars.peek().map(|&(_, n)| n);
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' && next == Some('"') => {
                chars.next();
                cur.push('"');
            }
            Some(_) => cur.push(c),
            None if c.is_whitespace() => {
                if let Some(st) = start.take() {
                    out.push(Word {
                        text: std::mem::take(&mut cur),
                        start: st,
                    });
                }
            }
            None => {
                start.get_or_insert(i);
                match c {
                    '"' | '\'' => quote = Some(c),
                    '\\' if next == Some(' ') => {
                        chars.next();
                        cur.push(' ');
                    }
                    _ => cur.push(c),
                }
            }
        }
    }
    if let Some(st) = start {
        out.push(Word { text: cur, start: st });
    }
    out
}

/// Byte ranges of the command segments separated by `&&`, `||`, `;`, `|`, `&` and newlines,
/// outside quotes. Ranges are trimmed of whitespace and grouping parentheses/braces.
/// Heredoc bodies and PowerShell here-strings are skipped.
pub fn split_segments(s: &str) -> Vec<Range<usize>> {
    Splitter::new(s).run()
}

struct Splitter<'a> {
    s: &'a str,
    b: &'a [u8],
    out: Vec<Range<usize>>,
    quote: Option<u8>,
    seg_start: usize,
    /// Heredoc terminators waiting for the end of the current line.
    pending: Vec<String>,
}

impl<'a> Splitter<'a> {
    fn new(s: &'a str) -> Self {
        Splitter {
            s,
            b: s.as_bytes(),
            out: Vec::new(),
            quote: None,
            seg_start: 0,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, a: usize, b: usize) {
        let (mut a, mut b) = (a, b);
        let bytes = self.b;
        while a < b && (bytes[a].is_ascii_whitespace() || matches!(bytes[a], b'(' | b'{')) {
            a += 1;
        }
        while b > a && (bytes[b - 1].is_ascii_whitespace() || matches!(bytes[b - 1], b')' | b'}')) {
            b -= 1;
        }
        if a < b {
            self.out.push(a..b);
        }
    }

    fn run(mut self) -> Vec<Range<usize>> {
        let mut i = 0;
        while i < self.b.len() {
            i = self.step(i);
        }
        self.push(self.seg_start, self.b.len());
        self.out
    }

    /// Handle the byte at `i`; returns the next position.
    fn step(&mut self, i: usize) -> usize {
        let c = self.b[i];
        // Heredocs (`<<EOF`, `<<-'EOF'`) count in any quote state, so one inside
        // "$(cat <<'EOF' ... )" is skipped too.
        if c == b'<' && self.b.get(i + 1) == Some(&b'<') && self.b.get(i + 2) != Some(&b'<') {
            if let Some((delim, used)) = heredoc_delimiter(&self.s[i + 2..]) {
                self.pending.push(delim);
                return i + 2 + used;
            }
        }
        if c == b'@' {
            if let Some(end) = here_string_end(self.s, i) {
                return end;
            }
        }
        if c == b'\n' && !self.pending.is_empty() {
            return self.skip_heredoc_bodies(i);
        }
        if let Some(q) = self.quote {
            if c == b'\\' && q == b'"' {
                return i + 2;
            }
            if c == q {
                self.quote = None;
            }
            return i + 1;
        }
        self.unquoted(i, c)
    }

    fn unquoted(&mut self, i: usize, c: u8) -> usize {
        match c {
            b'"' | b'\'' => self.quote = Some(c),
            b';' | b'\n' | b'\r' => {
                self.push(self.seg_start, i);
                self.seg_start = i + 1;
            }
            b'|' | b'&' => {
                let prev = if i > 0 { self.b[i - 1] } else { 0 };
                let next = self.b.get(i + 1).copied().unwrap_or(0);
                // Redirections such as 2>&1, &>file, >&2 are not separators.
                if c == b'&' && (prev == b'>' || prev == b'<' || next == b'>') {
                    return i + 1;
                }
                self.push(self.seg_start, i);
                let end = i + if next == c { 2 } else { 1 };
                self.seg_start = end;
                return end;
            }
            _ => {}
        }
        i + 1
    }

    /// At the newline ending a line with heredocs: skip each body up to its terminator.
    fn skip_heredoc_bodies(&mut self, nl: usize) -> usize {
        if self.quote.is_none() {
            self.push(self.seg_start, nl);
        }
        let mut pos = nl + 1;
        for d in std::mem::take(&mut self.pending) {
            while pos < self.b.len() {
                let end = self.s[pos..].find('\n').map_or(self.b.len(), |e| pos + e);
                let line = self.s[pos..end].trim_start_matches('\t').trim_end();
                pos = end + 1;
                if line == d {
                    break;
                }
            }
        }
        let next = pos.min(self.b.len());
        if self.quote.is_none() {
            self.seg_start = next;
        }
        next
    }
}

/// PowerShell here-string: `@'` / `@"` at a line end, closed by `'@` / `"@` at the start of a
/// later line. Returns the position after the close, or `None` if `i` does not start one.
fn here_string_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let q = *b.get(i + 1).filter(|q| matches!(q, b'\'' | b'"'))?;
    let line_end = i + 2 + s[i + 2..].find('\n')?;
    if !s[i + 2..line_end].trim().is_empty() {
        return None;
    }
    let close = [q, b'@'];
    let mut pos = line_end + 1;
    while pos < b.len() {
        if b[pos..].starts_with(&close) {
            return Some(pos + 2);
        }
        pos = s[pos..].find('\n').map_or(b.len(), |e| pos + e + 1);
    }
    Some(b.len())
}

/// The delimiter word after `<<` (with optional `-` and quotes) and the bytes it used.
fn heredoc_delimiter(rest: &str) -> Option<(String, usize)> {
    let b = rest.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let mut word = String::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            let close = rest[i + 1..].find(c as char)? + i + 1;
            word.push_str(&rest[i + 1..close]);
            i = close + 1;
        } else if c == b'\\' {
            i += 1;
        } else if c.is_ascii_whitespace() || b";|&<>()`".contains(&c) {
            break;
        } else {
            word.push(c as char);
            i += 1;
        }
    }
    (!word.is_empty()).then_some((word, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<&str> {
        split_segments(s).into_iter().map(|r| &s[r]).collect()
    }

    #[test]
    fn segments() {
        let s = "cd a && npx vitest run 2>&1 | tee \"x|y\"; echo 'a;b' & ls";
        assert_eq!(
            parts(s),
            vec!["cd a", "npx vitest run 2>&1", "tee \"x|y\"", "echo 'a;b'", "ls"]
        );
        assert_eq!(parts("(cd x && vitest)"), vec!["cd x", "vitest"]);
    }

    #[test]
    fn heredoc_bodies_are_not_commands() {
        assert_eq!(
            parts("cat > run.sh <<'EOF'\nnpm test\nnpx vitest run\nEOF\nls"),
            vec!["cat > run.sh <<'EOF'", "ls"]
        );
        let p = parts("git commit -m \"$(cat <<'EOF'\nfix: run npm test in CI\n\nnpx vitest \"x\"\nEOF\n)\" && git push");
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].starts_with("git commit"));
        assert_eq!(p[1], "git push");
        let p = parts("git commit -m @'\nfix: don't skip\n\neslint cleanup\n'@; npm test");
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].starts_with("git commit") && p[0].ends_with("'@"));
        assert_eq!(p[1], "npm test");
        assert_eq!(
            parts("cat <<-END >x\n\tcargo build\n\tEND\ncargo test"),
            vec!["cat <<-END >x", "cargo test"]
        );
    }

    #[test]
    fn word_offsets_follow_escapes() {
        let s = r#"FOO=a\ b BAR="x\"y z" npx jest"#;
        let w = words(s);
        assert_eq!(
            w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["FOO=a b", "BAR=x\"y z", "npx", "jest"]
        );
        assert_eq!(&s[w[2].start..], "npx jest");
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize(r#"a "b c" 'd e' C:\x\y "q\"t""#),
            vec!["a", "b c", "d e", r"C:\x\y", "q\"t"]
        );
    }
}
