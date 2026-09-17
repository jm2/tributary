//! Conservative reader for the dedicated OwnTone 29.3 configuration.
//!
//! This is an authority check, not a replacement for libconfuse. Accept a
//! documented subset; reject includes, expansion, duplicate options/sections,
//! escapes and unsupported library options rather than guessing their effect.
//! Upstream: d6fb3edf5831de38134ebd92fcf09a730ddd37aa, src/conffile.c
//! (sec_library), src/library/filescanner.c and owntone.conf.in.

use std::collections::BTreeMap;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Symbol(char),
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// A quoted value up to the closing `quote`. libconfuse escape/expansion
/// semantics are refused rather than interpreted.
fn quoted(chars: &mut Chars<'_>, quote: char) -> Option<String> {
    let mut value = String::new();
    loop {
        let next = chars.next()?;
        if next == quote {
            return Some(value);
        }
        if matches!(next, '\\' | '$' | '\n' | '\r') {
            return None;
        }
        value.push(next);
    }
}

/// A bare word (section or option name, or an unquoted scalar).
fn word(chars: &mut Chars<'_>, first: char) -> String {
    let mut word = String::from(first);
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
        word.push(c);
    }
    word
}

fn tokens(text: &str) -> Option<Vec<Token>> {
    let mut chars = text.chars().peekable();
    let mut result = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '#' => {
                chars.by_ref().find(|c| *c == '\n');
            }
            '{' | '}' | '=' | ',' => result.push(Token::Symbol(c)),
            '\'' | '"' => result.push(Token::Quoted(quoted(&mut chars, c)?)),
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => {
                result.push(Token::Word(word(&mut chars, c)));
            }
            _ => return None,
        }
    }
    Some(result)
}

#[derive(Debug)]
enum Value {
    Scalar(String),
    List(Vec<String>),
}

type Section = BTreeMap<String, Value>;

type Tokens = std::iter::Peekable<std::vec::IntoIter<Token>>;

/// The section names pinned OwnTone's `conffile.c` declares.
fn known_section(name: &str) -> bool {
    matches!(
        name,
        "general"
            | "library"
            | "audio"
            | "airplay_shared"
            | "fifo"
            | "spotify"
            | "sqlite"
            | "mpd"
            | "streaming"
    )
}

/// A quoted list after its opening `{`: `{ }` or `{ "a", "b" }`.
fn parse_list(it: &mut Tokens) -> Option<Vec<String>> {
    let mut list = Vec::new();
    if it.next_if_eq(&Token::Symbol('}')).is_some() {
        return Some(list);
    }
    loop {
        let Token::Quoted(value) = it.next()? else {
            return None;
        };
        list.push(value);
        match it.next()? {
            Token::Symbol('}') => return Some(list),
            Token::Symbol(',') => {}
            _ => return None,
        }
    }
}

/// One option value: a scalar or a quoted list.
fn parse_value(it: &mut Tokens) -> Option<Value> {
    match it.next()? {
        Token::Word(value) | Token::Quoted(value) => Some(Value::Scalar(value)),
        Token::Symbol('{') => parse_list(it).map(Value::List),
        Token::Symbol(_) => None,
    }
}

/// One section body after its opening `{`, up to and including its `}`.
/// Duplicate options are refused rather than resolved.
fn parse_section(it: &mut Tokens) -> Option<Section> {
    let mut options = BTreeMap::new();
    loop {
        let key = match it.next()? {
            Token::Symbol('}') => return Some(options),
            Token::Word(key) => key,
            _ => return None,
        };
        if it.next()? != Token::Symbol('=') {
            return None;
        }
        let value = parse_value(it)?;
        if options.insert(key, value).is_some() {
            return None;
        }
    }
}

fn parse(text: &str) -> Option<BTreeMap<String, Section>> {
    let mut it = tokens(text)?.into_iter().peekable();
    let mut sections = BTreeMap::new();
    while let Some(token) = it.next() {
        let Token::Word(section) = token else {
            return None;
        };
        if !known_section(&section) || it.next()? != Token::Symbol('{') {
            return None;
        }
        let options = parse_section(&mut it)?;
        if sections.insert(section, options).is_some() {
            return None;
        }
    }
    Some(sections)
}

pub(super) fn binds_pipe(config: &Path, pipe: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(config) else {
        return false;
    };
    let Some(sections) = parse(&text) else {
        return false;
    };
    let Some(library) = sections.get("library") else {
        return false;
    };
    library_options_acceptable(library) && pipe_is_the_scanned_input(library, pipe)
}

/// The `library` section binds only options whose input semantics we verify,
/// with the pipe/scanner values the adapter relies on.
fn library_options_acceptable(library: &Section) -> bool {
    library_keys_verified(library)
        && library_scalars_pinned(library)
        && library_filters_empty(library)
}

/// Restrict input-affecting options to ones whose semantics we verify.
fn library_keys_verified(library: &Section) -> bool {
    library.keys().all(|key| {
        matches!(
            key.as_str(),
            "name"
                | "port"
                | "password"
                | "directories"
                | "pipe_autostart"
                | "pipe_sample_rate"
                | "pipe_bits_per_sample"
                | "filescan_disable"
                | "follow_symlinks"
                | "filetypes_ignore"
                | "filepath_ignore"
        )
    })
}

/// The pipe/scanner scalars carry the values the adapter relies on (or their
/// defaults) and `follow_symlinks` is a plain boolean.
fn library_scalars_pinned(library: &Section) -> bool {
    for (key, expected) in [
        ("pipe_autostart", "true"),
        ("pipe_sample_rate", "44100"),
        ("pipe_bits_per_sample", "16"),
        ("filescan_disable", "false"),
    ] {
        if library
            .get(key)
            .is_some_and(|v| !matches!(v, Value::Scalar(s) if s == expected))
        {
            return false;
        }
    }
    library
        .get("follow_symlinks")
        .is_none_or(|v| matches!(v, Value::Scalar(s) if s == "true" || s == "false"))
}

/// Reject filters rather than reimplement POSIX regex/libconfuse matching.
fn library_filters_empty(library: &Section) -> bool {
    for key in ["filetypes_ignore", "filepath_ignore"] {
        if library
            .get(key)
            .is_some_and(|v| !matches!(v, Value::List(l) if l.is_empty()))
        {
            return false;
        }
    }
    true
}

/// The configured pipe is exactly the FIFO the scanner will find: a direct,
/// non-hidden `.pcm` child of the single absolute scanned directory.
fn pipe_is_the_scanned_input(library: &Section, pipe: &Path) -> bool {
    // Direct-child .pcm inputs avoid recursive scan, symlink and file-type
    // special cases (playlists/artwork/control files/hidden files).
    let Some(name) = pipe.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name.starts_with(['.', '_']) || pipe.extension().and_then(|s| s.to_str()) != Some("pcm") {
        return false;
    }
    // The scanned input must be the FIFO itself. A symlink, a regular file or
    // any other object at that pathname is refused (AM1): the adapter's PCM
    // writer is later bound to this same FIFO's identity.
    if !std::fs::symlink_metadata(pipe).is_ok_and(|m| m.file_type().is_fifo()) {
        return false;
    }
    let Some(parent) = pipe.parent().filter(|p| p.is_absolute()) else {
        return false;
    };
    let Ok(parent) = std::fs::canonicalize(parent) else {
        return false;
    };
    let Some(Value::List(dirs)) = library.get("directories") else {
        return false;
    };
    // One scanned directory, exactly the FIFO's parent. Relative directories
    // depend on the daemon's working directory and are deliberately refused.
    dirs.len() == 1
        && Path::new(&dirs[0]).is_absolute()
        && std::fs::canonicalize(&dirs[0]).is_ok_and(|dir| dir == parent)
}

#[cfg(test)]
pub(super) fn fixture(pipe: &Path) -> String {
    include_str!("../../tests/fixtures/owntone-29.3-library.conf").replace(
        "@PIPE_DIRECTORY@",
        pipe.parent()
            .expect("pipe parent")
            .to_str()
            .expect("UTF-8 path"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_library_config_binds_only_an_enabled_unfiltered_pcm_input() {
        let directory = tempfile::tempdir().unwrap();
        let pipe = directory.path().join("airplay.pcm");
        super::super::ensure_pipe(&pipe).unwrap();
        let config = directory.path().join("owntone.conf");
        let valid = fixture(&pipe);
        let check = |text: &str| {
            std::fs::write(&config, text).unwrap();
            binds_pipe(&config, &pipe)
        };
        assert!(check(&valid));
        assert!(check(&format!("general {{ uid = \"owntone\" }}\n{valid}")));
        assert!(check(
            &valid.replace("pipe_autostart = true", "# use the true default")
        ));
        for invalid in rejected_variants(&valid, directory.path(), &pipe) {
            assert!(!check(&invalid), "must reject {invalid}");
        }
        assert!(check(&valid));
        let hidden = directory.path().join("_hidden.pcm");
        super::super::ensure_pipe(&hidden).unwrap();
        assert!(!binds_pipe(&config, &hidden));
        assert!(!binds_pipe(&config, &directory.path().join("playlist.m3u")));
    }

    /// Every single-edit corruption of the pinned fixture that must be refused.
    fn rejected_variants(valid: &str, directory: &Path, pipe: &Path) -> Vec<String> {
        let dir = directory.to_str().unwrap();
        vec![
            valid.replace("library {", "general {"),
            valid.replace("pipe_autostart = true", "pipe_autostart = false"),
            valid.replace("44100", "48000"),
            valid.replace("16", "24"),
            valid.replace(
                "pipe_autostart = true",
                "pipe_autostart = true filescan_disable = true",
            ),
            valid.replace(
                "pipe_autostart = true",
                "pipe_autostart = true filepath_ignore = { \"pcm\" }",
            ),
            valid.replace(
                "pipe_autostart = true",
                "pipe_autostart = true filetypes_ignore = { \".pcm\" }",
            ),
            valid.replace(
                "pipe_autostart = true",
                "pipe_autostart = true pipe_autostart = false",
            ),
            valid.replace(
                "pipe_autostart = true",
                "pipe_autostart = true include(\"foreign.conf\")",
            ),
            valid.replace("directories =", "unknown ="),
            valid.replace(dir, "/nonexistent/other"),
            valid.replace(dir, "."),
            format!("{valid} {valid}"),
            format!("{valid} }}"),
            format!("# {}", valid.replace('\n', "\n# ")),
            format!("pipe_path = \"{}\"", pipe.display()),
        ]
    }

    /// Only the FIFO itself binds: a missing pathname, a regular file and a
    /// symlink (even one pointing at a FIFO) are refused.
    #[test]
    fn only_the_fifo_itself_binds() {
        let directory = tempfile::tempdir().unwrap();
        let pipe = directory.path().join("airplay.pcm");
        super::super::ensure_pipe(&pipe).unwrap();
        let config = directory.path().join("owntone.conf");
        std::fs::write(&config, fixture(&pipe)).unwrap();
        assert!(binds_pipe(&config, &pipe));
        std::fs::remove_file(&pipe).unwrap();
        assert!(!binds_pipe(&config, &pipe));
        std::fs::write(&pipe, b"not a fifo").unwrap();
        assert!(!binds_pipe(&config, &pipe));
        std::fs::remove_file(&pipe).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("airplay.pcm");
        super::super::ensure_pipe(&target).unwrap();
        std::os::unix::fs::symlink(&target, &pipe).unwrap();
        assert!(!binds_pipe(&config, &pipe));
    }

    #[test]
    fn quoted_hash_is_not_a_comment_and_ambiguous_syntax_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("input#one");
        std::fs::create_dir(&parent).unwrap();
        let pipe = parent.join("airplay.pcm");
        super::super::ensure_pipe(&pipe).unwrap();
        let config = directory.path().join("owntone.conf");
        std::fs::write(&config, fixture(&pipe)).unwrap();
        assert!(binds_pipe(&config, &pipe));
        for text in [
            "library {",
            "library { directories = { \"x\" }",
            "library {} garbage",
            "library {} =",
            "include(\"config\")",
            "library { name = \"$INPUT\" }",
        ] {
            assert!(parse(text).is_none(), "must reject {text}");
        }
    }
}
