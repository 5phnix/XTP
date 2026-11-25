//! The commit-message and pull-request conventions of CONTRIBUTING.md
//! ("Commits and pull request titles", "Provenance").
//!
//! A header is `<area>[/<topic>][, <area>[/<topic>]...]: <summary>`, at most
//! [`MAX_HEADER`] characters, with no trailing period. The areas are those of
//! [`AREAS`], the same names as the `area/...` labels of .github/labeler.yml.
//! The same rules apply to commit messages and to pull request titles (a pull
//! request is squashed into one commit whose message is its title).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const MAX_HEADER: usize = 72;

/// Every area a header may name, with what it covers (shown in errors).
pub const AREAS: &[(&str, &str)] = &[
    ("core", "crates/xtp-core"),
    ("pe", "crates/xtp-pe"),
    ("loader", "crates/xtp-loader"),
    ("hle", "crates/xtp-hle"),
    ("gpu", "crates/xtp-gpu"),
    ("shader", "crates/xtp-shader"),
    ("formats", "crates/xtp-formats"),
    ("pipeline", "crates/xtp-pipeline"),
    ("game", "crates/xtp-game"),
    ("testkit", "crates/xtp-testkit"),
    ("app", "apps/xtp"),
    ("xtask", "xtask"),
    ("tests", "tests/"),
    ("ci", ".github/, .githooks/"),
    ("docs", "docs/ and Markdown files"),
    ("deps", "dependency updates (Cargo.lock)"),
    (
        "build",
        "workspace manifest, toolchain, .cargo/, repository settings",
    ),
];

/// Prefixes git itself writes, followed by a header that must be valid.
const AUTOSQUASH: &[&str] = &["fixup! ", "squash! ", "amend! "];

/// Problems with one header (the first line of a message, or a PR title).
pub fn check_header(header: &str) -> Vec<String> {
    let mut header = header.trim_end_matches('\r');
    while let Some(rest) = AUTOSQUASH.iter().find_map(|p| header.strip_prefix(p)) {
        header = rest;
    }
    if let Some(inner) = header
        .strip_prefix("Revert \"")
        .and_then(|h| h.strip_suffix('"'))
    {
        return check_header(inner);
    }

    let mut problems = Vec::new();
    let Some((areas, summary)) = header.split_once(':') else {
        problems.push(format!("`{header}` has no `<area>: ` prefix"));
        return problems;
    };
    for area in areas.split(',') {
        let area = area.trim_start_matches(' ');
        let (name, topic) = area.split_once('/').unwrap_or((area, ""));
        if !AREAS.iter().any(|(a, _)| *a == name) {
            problems.push(format!("unknown area `{name}`"));
        }
        if area.split_once('/').is_some() && !is_topic(topic) {
            problems.push(format!(
                "topic `{topic}` after `{name}/` must be lowercase letters, digits, `-` or `_`"
            ));
        }
    }
    match summary.strip_prefix(' ') {
        None => problems.push("one space must follow the colon".to_owned()),
        Some(text) => {
            if text.trim().is_empty() {
                problems.push("the summary after the colon is empty".to_owned());
            } else if text.starts_with(' ') || text.ends_with(' ') {
                problems.push("the summary has extra spaces around it".to_owned());
            }
            if text.ends_with('.') {
                problems.push("the summary ends with a period".to_owned());
            }
            let first_word = text.split_whitespace().next().unwrap_or("");
            if first_word
                .trim_matches(|c: char| !c.is_alphanumeric())
                .eq_ignore_ascii_case("wip")
            {
                problems.push(
                    "work in progress goes in a draft pull request, not in a header".to_owned(),
                );
            }
        }
    }
    let len = header.chars().count();
    if len > MAX_HEADER {
        problems.push(format!("{len} characters, more than {MAX_HEADER}"));
    }
    if let Some(evidence) = french(header) {
        problems.push(not_english(&evidence));
    }
    problems
}

/// Accented letters of French that English text does not use.
const FRENCH_LETTERS: &[char] = &[
    'é', 'è', 'à', 'ç', 'ù', 'â', 'ê', 'î', 'ô', 'û', 'É', 'È', 'À', 'Ç', 'Ù', 'Â', 'Ê', 'Î', 'Ô',
    'Û',
];

/// French function words that are not English words: one is enough.
const FRENCH_WORDS: &[&str] = &[
    "les", "dans", "avec", "sont", "cette", "ces", "leur", "leurs", "mais", "donc", "quand",
    "aussi", "nous", "vous", "ainsi", "chaque", "puis", "selon",
];

/// French function words that an English text may hold on its own (a
/// cipher, an abbreviation, an LLVM predicate, a loanword): they count only
/// with a second sign of French on the same line.
const WEAK_FRENCH_WORDS: &[&str] = &[
    "une", "des", "est", "pour", "sans", "sur", "pas", "que", "qui", "du", "ne", "il", "le", "la",
    "un", "de", "et", "ou", "si", "au", "aux",
];

/// Why a line is rejected, with what gave it away.
fn not_english(evidence: &str) -> String {
    format!("{evidence} reads as French: commit messages, code comments and docs are in English")
}

/// Whether `line` reads as French; if so, the words that gave it away.
///
/// Left out: `Ported from <project> (commit) by <login>` lines and git
/// trailers (`Signed-off-by: ...`), which carry names; text between
/// backticks or double quotes (identifiers, quoted output); URLs and
/// `@login`s; capitalized words with accents (proper names); all-caps words
/// (acronyms).
pub fn french(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with("Ported from ") || is_trailer(line) {
        return None;
    }
    let mut strong = Vec::new();
    let mut weak = Vec::new();
    for chunk in unquoted(line).split_whitespace() {
        let chunk = chunk.trim_start_matches(|c: char| "([{<*".contains(c));
        if chunk.starts_with('@') || chunk.contains("://") {
            continue;
        }
        for word in chunk.split(|c: char| !(c.is_alphanumeric() || "_-'’".contains(c))) {
            let word = word.trim_matches(|c| c == '\'' || c == '’' || c == '-');
            if word.is_empty() {
                continue;
            }
            let lower = word.to_lowercase();
            let capitalized = word.chars().next().is_some_and(char::is_uppercase);
            let all_caps = word.chars().filter(|c| c.is_alphabetic()).count() > 1
                && !word.chars().any(char::is_lowercase);
            if all_caps {
                continue;
            }
            let lowercase_rest = word.chars().skip(1).all(|c| !c.is_uppercase());
            if !lowercase_rest {
                continue;
            }
            if is_elision(word) {
                strong.push(word.to_owned());
            } else if word.contains(FRENCH_LETTERS) {
                if !capitalized {
                    strong.push(word.to_owned());
                }
            } else if FRENCH_WORDS.contains(&lower.as_str()) {
                strong.push(word.to_owned());
            } else if WEAK_FRENCH_WORDS.contains(&lower.as_str()) {
                weak.push(word.to_owned());
            }
        }
    }
    let signs = strong.len() + weak.len();
    if strong.is_empty() && signs < 2 {
        return None;
    }
    let shown: Vec<String> = strong
        .into_iter()
        .chain(weak)
        .take(3)
        .map(|w| format!("`{w}`"))
        .collect();
    Some(shown.join(", "))
}

/// `c'est`, `d'un`, `l'adresse`, `qu'il`: a French elided article or pronoun.
fn is_elision(word: &str) -> bool {
    let Some((stem, rest)) = word.split_once(['\'', '’']) else {
        return false;
    };
    let stem = stem.to_lowercase();
    ["c", "d", "j", "l", "m", "n", "s", "t", "qu", "jusqu", "lorsqu", "puisqu"]
        .contains(&stem.as_str())
        && rest.chars().next().is_some_and(char::is_alphabetic)
        // `C's`, `T's`: an English possessive.
        && !rest.eq_ignore_ascii_case("s")
}

/// A git trailer such as `Signed-off-by: Name <mail>`.
fn is_trailer(line: &str) -> bool {
    line.split_once(": ").is_some_and(|(key, _)| {
        key.ends_with("-by") && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// `line` without what stands between backticks or double quotes.
fn unquoted(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut open: Option<char> = None;
    for c in line.chars() {
        match open {
            Some(q) if c == q => {
                open = None;
                out.push(' ');
            }
            Some(_) => {}
            None if matches!(c, '`' | '"' | '“' | '«') => {
                open = Some(match c {
                    '“' => '”',
                    '«' => '»',
                    other => other,
                });
            }
            None => out.push(c),
        }
    }
    out
}

/// Where a line of Rust source leaves the lexer, for the next line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lexer {
    #[default]
    Code,
    /// Inside a `"..."` string.
    Str,
    /// Inside a raw string with this many `#`.
    RawStr(usize),
    /// Inside a block comment, this deep.
    Block(usize),
}

/// The comment text of one line of Rust source (`//` to the end of the
/// line, or what lies inside block comments), given where the previous
/// line left `state`, which it updates. Strings and char literals are not
/// comments.
pub fn rust_comment(line: &str, state: &mut Lexer) -> Option<String> {
    let b = line.as_bytes();
    fn push(comment: &mut Option<String>, text: &str) {
        match comment {
            Some(c) => {
                c.push(' ');
                c.push_str(text);
            }
            None => *comment = Some(text.to_owned()),
        }
    }
    let mut comment: Option<String> = None;
    let mut block_start = 0;
    let mut i = 0;
    while i < b.len() {
        let rest = &b[i..];
        match *state {
            Lexer::Code => {
                if rest.starts_with(b"//") {
                    push(&mut comment, &line[i + 2..]);
                    return comment;
                } else if rest.starts_with(b"/*") {
                    *state = Lexer::Block(1);
                    i += 2;
                    block_start = i;
                    continue;
                } else if b[i] == b'"' {
                    *state = Lexer::Str;
                } else if b[i] == b'r' && raw_prefix(&b[..i]) {
                    let hashes = rest[1..].iter().take_while(|&&c| c == b'#').count();
                    if rest.get(1 + hashes) == Some(&b'"') {
                        *state = Lexer::RawStr(hashes);
                        i += 2 + hashes;
                        continue;
                    }
                } else if b[i] == b'\'' {
                    // A char literal (`'"'`, `'\''`, `'é'`) or a lifetime.
                    let after = &line[i + 1..];
                    // `'\x41'`: the quote that closes it comes after the
                    // escaped character.
                    let len = if let Some(escape) = after.strip_prefix('\\') {
                        escape.get(1..).and_then(|s| s.find('\'')).map(|n| n + 2)
                    } else {
                        after
                            .chars()
                            .next()
                            .map(char::len_utf8)
                            .filter(|&n| after[n..].starts_with('\''))
                    };
                    if let Some(len) = len {
                        i += len + 2;
                        continue;
                    }
                }
                i += 1;
            }
            Lexer::Str => {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    *state = Lexer::Code;
                }
                i += 1;
            }
            Lexer::RawStr(hashes) => {
                if b[i] == b'"' && rest[1..].iter().take_while(|&&c| c == b'#').count() >= hashes {
                    *state = Lexer::Code;
                    i += 1 + hashes;
                    continue;
                }
                i += 1;
            }
            Lexer::Block(depth) => {
                if rest.starts_with(b"*/") {
                    if depth == 1 {
                        push(&mut comment, &line[block_start..i]);
                        *state = Lexer::Code;
                    } else {
                        *state = Lexer::Block(depth - 1);
                    }
                    i += 2;
                    continue;
                }
                if rest.starts_with(b"/*") {
                    *state = Lexer::Block(depth + 1);
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }
    if matches!(state, Lexer::Block(_)) {
        push(&mut comment, &line[block_start.min(line.len())..]);
    }
    comment
}

/// Whether an `r` after `before` may start a raw string: it does not end
/// an identifier, or follows a lone `b` or `c` (`br"..."`, `cr"..."`).
fn raw_prefix(before: &[u8]) -> bool {
    let ident = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    match before {
        [] => true,
        [.., p, b'b' | b'c'] if !ident(p) => true,
        [b'b' | b'c'] => true,
        [.., p] => !ident(p),
    }
}

/// Problems with the Rust comments that a unified diff (`git diff -U0`)
/// adds: each added `.rs` line whose comment reads as French.
pub fn check_diff(diff: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut file: Option<&str> = None;
    let mut line_no = 0usize;
    // `-U0` shows no context: a string or block comment the hunk does not
    // open itself is not seen.
    let mut state = Lexer::Code;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").filter(|p| p.ends_with(".rs"));
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            // `@@ -a,b +c,d @@`: added lines are numbered from c.
            line_no = hunk
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|range| range.split(',').next()?.parse().ok())
                .unwrap_or(0);
            state = Lexer::Code;
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if let Some(path) = file
            && let Some(comment) = rust_comment(added, &mut state)
            && let Some(evidence) = french(&comment)
        {
            problems.push(format!(
                "{path}:{line_no}: comment `{}`: {}",
                comment.trim(),
                not_english(&evidence)
            ));
        }
        line_no += 1;
    }
    problems
}

/// Problems with a whole commit message: its header, and the blank line
/// that separates it from a body.
pub fn check_message(message: &str) -> Vec<String> {
    let mut lines = message.lines();
    let header = lines.next().unwrap_or("");
    if header.trim().is_empty() {
        return vec!["the message is empty or starts with a blank line".to_owned()];
    }
    let mut problems = check_header(header);
    if let Some(second) = lines.next()
        && !second.trim().is_empty()
    {
        problems.push("a blank line must separate the header from the body".to_owned());
    }
    for (i, line) in message.lines().enumerate().skip(1) {
        if let Some(evidence) = french(line) {
            problems.push(format!(
                "line {} of the message: {}",
                i + 1,
                not_english(&evidence)
            ));
        }
    }
    problems
}

/// A message as git hands it to the commit-msg hook: comment lines and
/// everything below the scissors line are not part of the commit.
pub fn strip_comments(message: &str) -> String {
    let mut out = String::new();
    for line in message.lines() {
        if line.starts_with("# ------------------------ >8") {
            break;
        }
        if !line.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_start_matches('\n').to_owned()
}

/// Problems with a pull request description: it must keep the template's
/// Provenance section with at least one ticked box.
pub fn check_body(body: &str) -> Vec<String> {
    let mut in_section = false;
    let mut found = false;
    let mut ticked = false;
    for line in body.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix('#') {
            in_section = heading
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("provenance");
            found |= in_section;
            continue;
        }
        if in_section {
            let lower = line.to_ascii_lowercase();
            ticked |= lower.starts_with("- [x]") || lower.starts_with("* [x]");
        }
    }
    if !found {
        vec![
            "the description has no `## Provenance` section (keep the pull request template)"
                .to_owned(),
        ]
    } else if !ticked {
        vec!["no box is ticked under `## Provenance`: state where the code comes from".to_owned()]
    } else {
        Vec::new()
    }
}

/// One commit of a range: its id and its message.
#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub message: String,
}

/// Parses `git log --format=%H%x00%B%x1e` output.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|record| {
            let (id, message) = record.trim_start_matches('\n').split_once('\0')?;
            Some(Commit {
                id: id.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect()
}

/// The commits in `base..head`, oldest first, merge commits left out (a
/// pull request is squashed, and merging `main` into a branch is allowed).
pub fn commits(root: &Path, base: &str, head: &str) -> Result<Vec<Commit>> {
    let out = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--reverse",
            "--format=%H%x00%B%x1e",
            &format!("{base}..{head}"),
        ])
        .current_dir(root)
        .output()
        .context("running git log")?;
    if !out.status.success() {
        bail!(
            "git log {base}..{head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_log(&String::from_utf8_lossy(&out.stdout)))
}

/// Problems with every commit of `base..head`, each prefixed with the short
/// commit id and its header, then with the Rust comments the range adds
/// (see [`check_diff`]). Also returns how many commits were checked.
pub fn check_range(root: &Path, base: &str, head: &str) -> Result<(usize, Vec<String>)> {
    let commits = commits(root, base, head)?;
    let mut problems = Vec::new();
    for c in &commits {
        let header = c.message.lines().next().unwrap_or("");
        let short = &c.id[..c.id.len().min(10)];
        for p in check_message(&c.message) {
            problems.push(format!("commit {short} `{header}`: {p}"));
        }
    }
    problems.extend(check_diff(&added_rust_lines(root, base, head)?));
    Ok((commits.len(), problems))
}

/// `git diff -U0` of the Rust files between where `head` left `base` and
/// `head`: what the range adds, not what `base` gained since.
fn added_rust_lines(root: &Path, base: &str, head: &str) -> Result<String> {
    let out = Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--no-textconv",
            "-U0",
            &format!("{base}...{head}"),
            "--",
            "*.rs",
        ])
        .current_dir(root)
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!(
            "git diff {base}...{head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The commit to compare a local branch against: where it left the first of
/// `upstream/main`, `origin/main` and `main` that exists.
pub fn default_base(root: &Path) -> Result<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    for candidate in ["upstream/main", "origin/main", "main"] {
        if git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{candidate}^{{commit}}"),
        ])
        .is_some()
        {
            return git(&["merge-base", "HEAD", candidate])
                .with_context(|| format!("no common ancestor of HEAD and {candidate}"));
        }
    }
    bail!("none of upstream/main, origin/main or main exists: pass --base <commit>")
}

fn is_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The rules and the areas, printed under header problems.
pub fn help() -> String {
    let mut out = format!(
        "A header is `<area>: <summary>` (or `<area>/<topic>: ...`, `<area>, <area>: ...`),\n\
         at most {MAX_HEADER} characters, without a final period. Areas:\n"
    );
    for (area, what) in AREAS {
        out.push_str(&format!("  {area:<9} {what}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(header: &str) {
        assert_eq!(check_header(header), Vec::<String>::new(), "{header}");
    }

    fn bad(header: &str, expected: &str) {
        let problems = check_header(header);
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{header}: expected a problem containing `{expected}`, got {problems:?}"
        );
    }

    #[test]
    fn accepts_conforming_headers() {
        ok("gpu: clamp indirect draw arguments");
        ok("hle/audio: decode Opus streams in parallel");
        ok("gpu, shader: translate 64-bit image atomics");
        ok("deps: Bump windows-sys from 0.61.2 to 0.62.0");
        ok("ci: bump the actions group with 2 updates");
        ok("fixup! loader: map title DLLs on first use");
        ok("Revert \"formats: detile 3D textures\"");
        ok(&format!("docs: {}", "x".repeat(MAX_HEADER - 6)));
    }

    #[test]
    fn rejects_malformed_headers() {
        bad("Fix the loader", "no `<area>: ` prefix");
        bad("renderer: fix it", "unknown area `renderer`");
        bad("xtp-gpu: fix it", "unknown area `xtp-gpu`");
        bad("gpu:fix it", "one space must follow");
        bad("gpu: ", "summary after the colon is empty");
        bad("gpu: fix it.", "ends with a period");
        bad("gpu: WIP indirect draws", "draft pull request");
        bad("gpu/Draws: fix", "topic `Draws`");
        bad("gpu/: fix", "topic ``");
        bad(
            &format!("docs: {}", "x".repeat(MAX_HEADER - 5)),
            "more than 72",
        );
        bad("Revert \"no area here\"", "no `<area>: ` prefix");
        bad("fixup! bad", "no `<area>: ` prefix");
    }

    #[test]
    fn checks_the_blank_line_before_the_body() {
        assert!(check_message("core: add x\n\nWhy it is needed.\n").is_empty());
        assert!(check_message("core: add x\n").is_empty());
        assert!(!check_message("core: add x\nWhy it is needed.\n").is_empty());
        assert!(!check_message("\ncore: add x\n").is_empty());
        assert!(!check_message("").is_empty());
    }

    #[test]
    fn accepts_english_text() {
        for line in [
            "gpu: clamp indirect draw arguments",
            "The fcmp predicate une is unordered or not equal; oeq is ordered.",
            "Decrypt with DES, then compare (EST timestamps).",
            "Ported from Xenia (0123abcd) by @josé-dev",
            "Signed-off-by: José Núñez <jose@example.org>",
            "Thanks to Clément and Hervé for the trace.",
            "The title logs `les registres` and \"une erreur est survenue\".",
            "See https://example.org/les-pages-de-garde for the layout.",
            "A naïve copy is on par with du -sh.",
            "It's the lane's address; don't read the user's copy.",
            "Pour the samples into the ring buffer.",
            "Sans-serif glyphs come from the font atlas.",
            "The dumps of session C's run and of the L's corner.",
        ] {
            assert_eq!(french(line), None, "{line}");
        }
        ok("shader: lower une and ueq fcmp predicates");
        assert!(check_message("core: add x\n\nPorted from Xenia (0123abcd) by @josé\n").is_empty());
    }

    #[test]
    fn rejects_french_text() {
        for (line, word) in [
            ("corrige les draws indirects", "`les`"),
            ("SAFETY: la passe lit la sortie via une UAV.", "`une`"),
            ("Une page de garde du titre que la copie lit", "`Une`"),
            ("lire uniquement si le premier dword est accessible", "`si`"),
            (
                "Pour un TBUFFER sur SRV, le format vient de l'instruction.",
                "`l'instruction`",
            ),
            ("Un SRV racine n'a pas de borne D3D.", "`n'a`"),
            ("garde déjà posée", "`déjà`"),
            ("Dans ce cas, rien à faire", "`Dans`"),
        ] {
            let found = french(line).unwrap_or_default();
            assert!(found.contains(word), "{line}: {found:?}");
        }
        bad("gpu: corrige les draws indirects", "reads as French");
        bad("hle: unité de copie", "`unité`");
        let message = "core: add x\n\nThe copy runs first.\nPuis la garde est posée.\n";
        let problems = check_message(message);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("line 4 of the message"),
            "{problems:?}"
        );
    }

    #[test]
    fn finds_the_comment_of_a_rust_line() {
        let comment = |line: &str| rust_comment(line, &mut Lexer::Code);
        assert_eq!(comment("let x = 1; // one").as_deref(), Some(" one"));
        assert_eq!(comment("    /// doc").as_deref(), Some("/ doc"));
        assert_eq!(
            comment("let a = 1; /* b */ let c = 2;").as_deref(),
            Some(" b ")
        );
        assert_eq!(comment("let url = \"https://les.example\";"), None);
        assert_eq!(
            comment("let s = \"a \\\" // b\"; // c").as_deref(),
            Some(" c")
        );
        assert_eq!(
            comment("let r = r#\"a\" // b\"#; // c").as_deref(),
            Some(" c")
        );
        assert_eq!(
            comment("let q = ['\"', '\\'', '\\\\', 'é']; // d").as_deref(),
            Some(" d")
        );
        assert_eq!(
            comment("fn f<'a>(x: &'a str) {} // e").as_deref(),
            Some(" e")
        );

        // A string or a block comment that goes on to the next lines.
        let mut state = Lexer::Code;
        assert_eq!(rust_comment("let s = \"one", &mut state), None);
        assert_eq!(rust_comment("// still the string", &mut state), None);
        assert_eq!(
            rust_comment("end\"; /* open", &mut state).as_deref(),
            Some(" open")
        );
        assert_eq!(
            rust_comment("  inside", &mut state).as_deref(),
            Some("  inside")
        );
        assert_eq!(
            rust_comment("close */ x();", &mut state).as_deref(),
            Some("close ")
        );
        assert_eq!(state, Lexer::Code);
    }

    #[test]
    fn checks_the_comments_a_diff_adds() {
        let diff = "\
diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs
--- a/crates/a/src/lib.rs
+++ b/crates/a/src/lib.rs
@@ -3,2 +3,0 @@
-// Lit les registres du titre.
-// Une page de garde.
@@ -10,0 +8,4 @@
+// Reads the title's registers.
+let msg = \"les registres sont lus\"; // logged as is
+    /// Copie les pages dans la vue.
+fn une() {}
diff --git a/docs/notes.md b/docs/notes.md
--- a/docs/notes.md
+++ b/docs/notes.md
@@ -1,0 +1 @@
+// Les notes en français.
diff --git a/crates/a/src/new.rs b/crates/a/src/new.rs
--- /dev/null
+++ b/crates/a/src/new.rs
@@ -0,0 +1,2 @@
+//! Module d'entrée.
+pub fn f() {}
";
        let problems = check_diff(diff);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].starts_with("crates/a/src/lib.rs:10:"),
            "{problems:?}"
        );
        assert!(problems[0].contains("`les`"), "{problems:?}");
        assert!(
            problems[1].starts_with("crates/a/src/new.rs:1:"),
            "{problems:?}"
        );
        assert!(check_diff("").is_empty());
    }

    #[test]
    fn strips_what_git_does_not_commit() {
        let edited = "core: add x\n\nbody\n# Please enter the commit message\n\
                      # ------------------------ >8 ------------------------\ndiff --git a b\n";
        assert_eq!(strip_comments(edited), "core: add x\n\nbody\n");
    }

    #[test]
    fn requires_a_ticked_provenance_box() {
        let template = include_str!("../../.github/pull_request_template.md");
        assert!(
            !check_body(template).is_empty(),
            "the untouched template must not pass"
        );
        let first_box = template.replacen("- [ ]", "- [x]", 1);
        assert!(
            !check_body(&first_box).is_empty(),
            "a box ticked outside Provenance does not count"
        );
        let (before, after) = template
            .split_once("## Provenance")
            .expect("the template has a Provenance section");
        let filled = format!(
            "{before}## Provenance{}",
            after.replacen("- [ ]", "- [x]", 1)
        );
        assert_eq!(check_body(&filled), Vec::<String>::new());
        assert!(!check_body("Fixes the loader.").is_empty());
    }

    #[test]
    fn parses_git_log_records() {
        let out = "aaa\0core: one\n\nbody\n\x1e\nbbb\0gpu: two\n\x1e\n";
        assert_eq!(
            parse_log(out),
            vec![
                Commit {
                    id: "aaa".into(),
                    message: "core: one\n\nbody\n".into()
                },
                Commit {
                    id: "bbb".into(),
                    message: "gpu: two\n".into()
                },
            ]
        );
    }

    /// The areas are the `area/...` labels the labeler applies, and each
    /// crate of the workspace has its area.
    #[test]
    fn areas_match_the_labeler_and_the_crates() {
        let labeler = include_str!("../../.github/labeler.yml");
        let mut labels: Vec<&str> = labeler
            .lines()
            .filter_map(|l| l.strip_prefix("area/"))
            .filter_map(|l| l.strip_suffix(':'))
            .collect();
        let mut areas: Vec<&str> = AREAS.iter().map(|(a, _)| *a).collect();
        labels.sort_unstable();
        areas.sort_unstable();
        assert_eq!(labels, areas);

        let crates = crate::workspace_root().join("crates");
        for entry in std::fs::read_dir(crates).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let area = name.strip_prefix("xtp-").expect("crates are named xtp-*");
            assert!(areas.contains(&area), "crate {name} has no area");
            assert!(
                labeler.contains(&format!("'crates/{name}/**'")),
                "crate {name} has no labeler glob"
            );
        }
    }

    /// A directory under the system's temporary folder, removed on drop.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("xtask-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Checks a real range of commits in a scratch repository.
    #[test]
    fn checks_a_range_of_commits() {
        let dir = ScratchDir::new("conventions-range");
        let hooks = dir.path().join("no-hooks");
        std::fs::create_dir(&hooks).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-c", "user.name=xtask", "-c", "user.email=xtask@invalid"])
                .args(["-c", "commit.gpgsign=false", "-c"])
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(status.status.success(), "git {args:?}: {status:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "Initial commit"]);
        git(&["checkout", "-q", "-b", "topic"]);
        git(&["commit", "-q", "--allow-empty", "-m", "core: add a thing"]);
        git(&["commit", "-q", "--allow-empty", "-m", "Fix stuff."]);

        let (count, problems) = check_range(dir.path(), "main", "topic").unwrap();
        assert_eq!(count, 2, "the commit on main is not in the range");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`Fix stuff.`"), "{problems:?}");
        assert!(check_range(dir.path(), "main", "no-such-branch").is_err());

        // French on main before the branch left it is not the range's.
        git(&["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("old.rs"), "// Les gardes.\n").unwrap();
        git(&["add", "old.rs"]);
        git(&["commit", "-q", "-m", "core: add the old file"]);
        git(&["checkout", "-q", "-b", "french", "main"]);
        std::fs::write(
            dir.path().join("lib.rs"),
            "// Reads the guard.\nfn f() {} // pour chaque page\n",
        )
        .unwrap();
        git(&["add", "lib.rs"]);
        git(&["commit", "-q", "-m", "core: corrige les gardes"]);
        git(&["checkout", "-q", "main"]);
        git(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "core: après la branche",
        ]);

        let (count, problems) = check_range(dir.path(), "main", "french").unwrap();
        assert_eq!(count, 1);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("`core: corrige les gardes`"),
            "{problems:?}"
        );
        assert!(problems[1].starts_with("lib.rs:2: "), "{problems:?}");
    }
}
