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

fn tokens(text: &str) -> Option<Vec<Token>> {
    let mut chars = text.chars().peekable();
    let mut result = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '{' | '}' | '=' | ',' => result.push(Token::Symbol(c)),
            '\'' | '"' => {
                let mut value = String::new();
                loop {
                    let next = chars.next()?;
                    if next == c {
                        break;
                    }
                    // Do not interpret libconfuse escape/expansion semantics.
                    if matches!(next, '\\' | '$' | '\n' | '\r') {
                        return None;
                    }
                    value.push(next);
                }
                result.push(Token::Quoted(value));
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => {
                let mut word = String::from(c);
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    word.push(chars.next()?);
                }
                result.push(Token::Word(word));
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

fn parse(text: &str) -> Option<BTreeMap<String, Section>> {
    let tokens = tokens(text)?;
    let mut it = tokens.into_iter().peekable();
    let mut sections = BTreeMap::new();
    while let Some(token) = it.next() {
        let Token::Word(section) = token else {
            return None;
        };
        if !matches!(
            section.as_str(),
            "general"
                | "library"
                | "audio"
                | "airplay_shared"
                | "fifo"
                | "spotify"
                | "sqlite"
                | "mpd"
                | "streaming"
        ) {
            return None;
        }
        if it.next()? != Token::Symbol('{') {
            return None;
        }
        let mut options = BTreeMap::new();
        loop {
            let key = match it.next()? {
                Token::Symbol('}') => break,
                Token::Word(key) => key,
                _ => return None,
            };
            if it.next()? != Token::Symbol('=') {
                return None;
            }
            let value = match it.next()? {
                Token::Word(value) | Token::Quoted(value) => Value::Scalar(value),
                Token::Symbol('{') => {
                    let mut list = Vec::new();
                    if it.peek() == Some(&Token::Symbol('}')) {
                        it.next();
                    } else {
                        loop {
                            let Token::Quoted(value) = it.next()? else {
                                return None;
                            };
                            list.push(value);
                            match it.next()? {
                                Token::Symbol('}') => break,
                                Token::Symbol(',') => {}
                                _ => return None,
                            }
                        }
                    }
                    Value::List(list)
                }
                Token::Symbol(_) => return None,
            };
            if options.insert(key, value).is_some() {
                return None;
            }
        }
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
    // Restrict input-affecting options to ones whose semantics we verify.
    if library.keys().any(|key| {
        !matches!(
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
    }) {
        return false;
    }
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
    if library
        .get("follow_symlinks")
        .is_some_and(|v| !matches!(v, Value::Scalar(s) if s == "true" || s == "false"))
    {
        return false;
    }
    // Reject filters rather than reimplement POSIX regex/libconfuse matching.
    for key in ["filetypes_ignore", "filepath_ignore"] {
        if library
            .get(key)
            .is_some_and(|v| !matches!(v, Value::List(l) if l.is_empty()))
        {
            return false;
        }
    }
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
        for invalid in [
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
            valid.replace(directory.path().to_str().unwrap(), "/nonexistent/other"),
            valid.replace(directory.path().to_str().unwrap(), "."),
            format!("{valid} {valid}"),
            format!("{valid} }}"),
            format!("# {}", valid.replace('\n', "\n# ")),
            format!("pipe_path = \"{}\"", pipe.display()),
        ] {
            assert!(!check(&invalid), "must reject {invalid}");
        }
        assert!(check(&valid));
        let hidden = directory.path().join("_hidden.pcm");
        super::super::ensure_pipe(&hidden).unwrap();
        assert!(!binds_pipe(&config, &hidden));
        assert!(!binds_pipe(&config, &directory.path().join("playlist.m3u")));
        // Only the FIFO itself binds: a missing pathname, a regular file and
        // a symlink (even one pointing at a FIFO) are refused.
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
