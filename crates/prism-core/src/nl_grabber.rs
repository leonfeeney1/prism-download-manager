//! Natural-language grabber input: a local, rule-based parser that turns a
//! plain-English sentence into a [`GrabberConfig`].
//!
//! There is no AI model, no API key and no network call here: the parser only
//! matches words, extensions and numbers in the sentence itself. It starts
//! from [`GrabberConfig::default`] and overrides only what the sentence says,
//! so anything it does not mention keeps the grabber's normal defaults.
//!
//! The UI must show the parsed config as an editable preview before starting
//! anything: this function never starts a crawl, and it refuses to guess. A
//! sentence with no website address fails with [`ParseError::MissingUrl`];
//! one with an address but nothing recognizable to collect fails with
//! [`ParseError::MissingTarget`].

use crate::grabber::{GrabberConfig, Scope};
use std::fmt;

/// What the parser understood from one sentence: a complete grabber config.
pub type ParsedGrab = GrabberConfig;

/// Why a sentence could not be turned into a grabber project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No website address was found in the sentence.
    MissingUrl,
    /// An address was found, but nothing recognizable to collect.
    MissingTarget,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MissingUrl => write!(
                f,
                "I could not find a website address in that sentence. \
                 Add a URL like https://example.com/ and try again."
            ),
            ParseError::MissingTarget => write!(
                f,
                "I found the site, but not what to grab. Say what to collect, \
                 for example PDFs, images, videos, or everything."
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// One file-type group: the words that name it, and the extensions it means.
/// These mirror the presets in the grabber UI (images, video, audio,
/// documents, archives, programs), plus PDFs on their own.
struct TypeGroup {
    words: &'static [&'static str],
    extensions: &'static [&'static str],
}

const TYPE_GROUPS: &[TypeGroup] = &[
    TypeGroup {
        words: &["image", "images", "picture", "pictures", "photo", "photos", "pic", "pics"],
        extensions: &["jpg", "jpeg", "png", "gif", "webp", "avif", "svg", "tif", "tiff", "bmp"],
    },
    TypeGroup {
        words: &["video", "videos", "film", "films", "movie", "movies", "clip", "clips"],
        extensions: &["mp4", "mkv", "webm", "mov", "avi", "m4v"],
    },
    TypeGroup {
        words: &["audio", "music", "song", "songs", "podcast", "podcasts"],
        extensions: &["mp3", "m4a", "flac", "wav", "ogg", "opus"],
    },
    TypeGroup {
        words: &[
            "document",
            "documents",
            "doc",
            "docs",
            "word",
            "excel",
            "spreadsheet",
            "spreadsheets",
            "presentation",
            "presentations",
            "ebook",
            "ebooks",
        ],
        extensions: &["pdf", "epub", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt"],
    },
    TypeGroup {
        words: &["archive", "archives", "compressed", "package", "packages"],
        extensions: &["zip", "rar", "7z", "tar", "gz", "xz", "zst"],
    },
    TypeGroup {
        words: &["program", "programs", "installer", "installers", "software", "app", "apps"],
        extensions: &["exe", "msi", "dmg", "deb", "rpm", "appimage", "iso"],
    },
    TypeGroup {
        words: &["pdf", "pdfs"],
        extensions: &["pdf"],
    },
];

/// Extensions that may also appear on their own (`.mp3`, `mkv`, …) without
/// naming a whole group.
const KNOWN_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "svg", "tif", "tiff", "bmp", "mp4", "mkv", "webm",
    "mov", "avi", "m4v", "mp3", "m4a", "flac", "wav", "ogg", "opus", "pdf", "epub", "doc", "docx",
    "xls", "xlsx", "ppt", "pptx", "txt", "zip", "rar", "7z", "tar", "gz", "xz", "zst", "exe", "msi",
    "dmg", "deb", "rpm", "appimage", "iso",
];

const NUMBER_WORDS: &[(&str, u32)] = &[
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
];

/// Punctuation stripped from the ends of a token (sentence-final periods and
/// commas, brackets, quotes). Internal dots (as in `example.com`) are kept.
const TOKEN_TRIM: &[char] = &['(', '[', '{', '<', '"', '\'', '.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '"', '\''];

fn clean_token(token: &str) -> &str {
    token.trim_matches(TOKEN_TRIM)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `word` appears in `text` as a whole word, not inside another.
fn contains_word(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_char(text[..abs].chars().last().unwrap());
        let after = abs + word.len();
        let after_ok = after >= text.len() || !is_word_char(text[after..].chars().next().unwrap());
        if before_ok && after_ok {
            return true;
        }
        start = abs + word.len();
    }
    false
}

/// Split into lowercase words (letters, digits and `.` kept so `example.com`
/// and `.pdf` stay intact; everything else is a separator).
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '/'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

/// A known extension named by `word`, allowing a trailing plural `s`
/// ("mp3s", "gifs") and a leading dot (".mkv").
fn extension_named(word: &str) -> Option<&'static str> {
    let base = word.strip_prefix('.').unwrap_or(word);
    if let Some(e) = KNOWN_EXTENSIONS.iter().find(|e| **e == base) {
        return Some(e);
    }
    if let Some(singular) = base.strip_suffix('s') {
        if let Some(e) = KNOWN_EXTENSIONS.iter().find(|e| **e == singular) {
            return Some(e);
        }
    }
    None
}

/// True when `item` names a type group, allowing a trailing plural `s`.
fn group_named(item: &str) -> Option<&'static TypeGroup> {
    let singular = item.strip_suffix('s').unwrap_or(item);
    TYPE_GROUPS
        .iter()
        .find(|g| g.words.contains(&item) || g.words.contains(&singular))
}

fn parse_number(word: &str) -> Option<u32> {
    if let Ok(n) = word.parse::<u32>() {
        return Some(n);
    }
    NUMBER_WORDS
        .iter()
        .find(|(w, _)| *w == word)
        .map(|(_, n)| *n)
}

/// Find the first URL-like token. Returns (normalized URL, raw token as
/// typed) so the caller can remove the token from the sentence before
/// scanning it for type words (a path like `/images/` must not count as
/// "images" to collect).
fn find_url(input: &str) -> Option<(String, String)> {
    for token in input.split_whitespace() {
        let cleaned = clean_token(token);
        if cleaned.contains('@') {
            continue; // an email address, not a site
        }
        let lower = cleaned.to_ascii_lowercase();
        let bare = lower
            .strip_prefix("https://")
            .or_else(|| lower.strip_prefix("http://"))
            .or_else(|| lower.strip_prefix("www."))
            .unwrap_or(&lower);
        // The host is the part before the first slash; it needs a dot whose
        // suffix is letters (not a known file extension like `.pdf`).
        let host = bare.split('/').next().unwrap_or("");
        let suffix = host.rsplit('.').next().unwrap_or("");
        let looks_like_host = host.contains('.')
            && suffix.len() >= 2
            && suffix.chars().all(|c| c.is_ascii_alphabetic())
            && !KNOWN_EXTENSIONS.contains(&suffix);
        if !looks_like_host {
            continue;
        }
        let normalized = if lower.starts_with("http://") || lower.starts_with("https://") {
            cleaned.to_string()
        } else {
            format!("https://{cleaned}")
        };
        return Some((normalized, token.to_string()));
    }
    None
}

/// Exclusion clauses: "but not X", "excluding X", "except X", "skip X",
/// "without X", "not including X". Returns (remaining text, removed type
/// groups' extensions, extra exclude substrings).
fn split_exclusions(text: &str) -> (String, Vec<String>, Vec<String>) {
    const MARKERS: &[&str] = &[
        "but not",
        "not including",
        "excluding",
        "except",
        "skip",
        "without",
    ];
    let mut cut = text.len();
    for marker in MARKERS {
        if let Some(pos) = find_phrase(text, marker) {
            cut = cut.min(pos);
        }
    }
    // A trailing ", no X" clause ("all images, no gifs").
    if let Some(pos) = text.find(", no ") {
        cut = cut.min(pos);
    }
    if cut >= text.len() {
        return (text.to_string(), Vec::new(), Vec::new());
    }
    let (kept, clause) = text.split_at(cut);
    // The clause starts with its marker (or ", no "); drop that prefix.
    let mut body = clause;
    for marker in MARKERS {
        if let Some(rest) = body.strip_prefix(marker) {
            body = rest;
            break;
        }
    }
    if let Some(rest) = body.strip_prefix(", no ") {
        body = rest;
    }
    let mut removed_exts: Vec<String> = Vec::new();
    let mut extra_exclude: Vec<String> = Vec::new();
    for part in body.split([',']) {
        // "A and B" inside one comma part counts as two items.
        for item in part.split(" and ") {
            let item = item.trim().trim_matches(TOKEN_TRIM).trim();
            let item = item.strip_prefix("the ").unwrap_or(item);
            if item.is_empty() {
                continue;
            }
            // A type word (or a single known extension) removes those files
            // instead of becoming an exclude substring.
            let mut matched_type = false;
            if let Some(group) = group_named(item) {
                removed_exts.extend(group.extensions.iter().map(|e| e.to_string()));
                matched_type = true;
            }
            if !matched_type {
                if let Some(ext) = extension_named(item) {
                    removed_exts.push(ext.to_string());
                } else {
                    // Keep only the first word: "logout pages" means `logout`.
                    let first = item.split_whitespace().next().unwrap_or(item);
                    extra_exclude.push(first.to_string());
                }
            }
        }
    }
    (kept.to_string(), removed_exts, extra_exclude)
}

/// Find `phrase` as whole words in `text`; returns its byte position.
fn find_phrase(text: &str, phrase: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(pos) = text[start..].find(phrase) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_char(text[..abs].chars().last().unwrap());
        let after = abs + phrase.len();
        let after_ok = after >= text.len() || !is_word_char(text[after..].chars().next().unwrap());
        if before_ok && after_ok {
            return Some(abs);
        }
        start = abs + phrase.len();
    }
    None
}

/// Depth from wording: "two pages deep" / "depth 3" / "this page" (0) /
/// "the whole site" (10, the grabber UI's maximum) / default 2.
fn parse_depth(text: &str, toks: &[String]) -> u32 {
    // Explicit "N pages/levels deep" or "depth N".
    for w in toks.windows(3) {
        if let Some(n) = parse_number(&w[0])
            && (w[1] == "pages" || w[1] == "levels" || w[1] == "page" || w[1] == "level")
            && w[2] == "deep"
        {
            return n.min(10);
        }
    }
    for w in toks.windows(2) {
        if w[0] == "depth"
            && let Some(n) = parse_number(&w[1])
        {
            return n.min(10);
        }
    }
    if contains_word(text, "this page") || contains_word(text, "single page") {
        return 0;
    }
    if contains_word(text, "whole site")
        || contains_word(text, "entire site")
        || contains_word(text, "whole website")
        || contains_word(text, "entire website")
        || contains_word(text, "all pages")
    {
        return 10;
    }
    GrabberConfig::default().depth
}

/// Page limit from wording: "up to 50 pages", "at most 50 pages",
/// "max 50 pages", "50 pages max". Defaults to the grabber default.
fn parse_max_pages(toks: &[String]) -> u32 {
    let default = GrabberConfig::default().max_pages;
    // Look for a number next to the word "pages" with a limiting word nearby.
    for (i, tok) in toks.iter().enumerate() {
        if tok != "pages" {
            continue;
        }
        let prev = toks.get(i.wrapping_sub(1)).map(|s| s.as_str()).unwrap_or("");
        let prev2 = toks.get(i.wrapping_sub(2)).map(|s| s.as_str()).unwrap_or("");
        let next = toks.get(i + 1).map(|s| s.as_str()).unwrap_or("");
        let num = parse_number(prev)
            .filter(|_| prev2 == "to" || prev2 == "most" || prev2 == "max" || prev2 == "maximum" || prev2 == "first" || prev == "max" || prev == "maximum" || prev == "first")
            .or_else(|| {
                if next == "max" || next == "maximum" {
                    parse_number(prev)
                } else {
                    None
                }
            });
        if let Some(n) = num {
            return n.clamp(1, 100_000);
        }
    }
    default
}

/// Parse one plain-English sentence into a grabber config. Never starts
/// anything: the caller shows the result for review first.
pub fn parse_natural_grabber(input: &str) -> Result<ParsedGrab, ParseError> {
    let (start_url, raw_token) = find_url(input).ok_or(ParseError::MissingUrl)?;
    // Work on the lowercased sentence with the URL token removed, so words
    // inside the address itself (e.g. `/images/`) are not mistaken for types.
    let lowered = input.to_ascii_lowercase();
    let without_url = lowered.replacen(&raw_token.to_ascii_lowercase(), " ", 1);
    let (kept, removed_exts, extra_exclude) = split_exclusions(&without_url);
    let toks = words(&kept);

    // File types, in group order; explicit single extensions add one ext.
    let mut extensions: Vec<String> = Vec::new();
    let mut media = false;
    for (i, group) in TYPE_GROUPS.iter().enumerate() {
        if group.words.iter().any(|w| contains_word(&kept, w)) {
            extensions.extend(group.extensions.iter().map(|e| e.to_string()));
            if i <= 2 {
                media = true; // images, video, audio live in page markup too
            }
        }
    }
    for tok in &toks {
        if let Some(ext) = extension_named(tok) {
            if !extensions.contains(&ext.to_string()) {
                extensions.push(ext.to_string());
            }
        }
    }
    let everything = contains_word(&kept, "everything")
        || contains_word(&kept, "all files")
        || contains_word(&kept, "all downloads")
        || contains_word(&kept, "every file")
        || contains_word(&kept, "anything");
    if !removed_exts.is_empty() {
        let kept_exts: Vec<String> = extensions
            .iter()
            .filter(|e| !removed_exts.contains(e))
            .cloned()
            .collect();
        // A removal that would empty the list is ignored: "pdfs but not
        // pdfs" must not silently become "any file".
        if !kept_exts.is_empty() {
            extensions = kept_exts;
        }
    }
    extensions.sort();
    extensions.dedup();
    if extensions.is_empty() && !everything {
        return Err(ParseError::MissingTarget);
    }
    if everything {
        extensions.clear();
        media = true;
    }

    let depth = parse_depth(&kept, &toks);
    let scope = if contains_word(&kept, "this folder") || contains_word(&kept, "this directory") {
        Scope::SubPath
    } else if contains_word(&kept, "any site") || contains_word(&kept, "anywhere") {
        Scope::Any
    } else if contains_word(&kept, "whole site")
        || contains_word(&kept, "entire site")
        || contains_word(&kept, "same site")
    {
        Scope::SameDomain
    } else {
        Scope::SameHost
    };

    let mut exclude = GrabberConfig::default().exclude;
    for e in extra_exclude {
        if !exclude.contains(&e) {
            exclude.push(e);
        }
    }
    Ok(GrabberConfig {
        start_url,
        extensions,
        include_embedded: media,
        depth,
        scope,
        max_pages: parse_max_pages(&toks),
        exclude,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdfs_from_a_site() {
        let cfg = parse_natural_grabber("download all PDFs from example.com").unwrap();
        assert_eq!(cfg.start_url, "https://example.com");
        assert_eq!(cfg.extensions, vec!["pdf"]);
        assert!(!cfg.include_embedded);
        assert_eq!(cfg.depth, 2);
    }

    #[test]
    fn images_on_one_page() {
        let cfg =
            parse_natural_grabber("grab every image on this page https://example.com/gallery")
                .unwrap();
        assert_eq!(cfg.start_url, "https://example.com/gallery");
        assert_eq!(cfg.depth, 0);
        assert!(cfg.include_embedded);
        assert!(cfg.extensions.contains(&"jpg".to_string()));
        assert!(cfg.extensions.contains(&"png".to_string()));
    }

    #[test]
    fn url_words_do_not_count_as_types() {
        // "images" lives in the address only; the sentence names PDFs.
        let cfg = parse_natural_grabber("download the pdfs from example.com/images/").unwrap();
        assert_eq!(cfg.extensions, vec!["pdf"]);
    }

    #[test]
    fn depth_wording_digits_and_words() {
        let cfg = parse_natural_grabber(
            "download all videos from https://example.com, three pages deep",
        )
        .unwrap();
        assert_eq!(cfg.depth, 3);
        assert!(cfg.extensions.contains(&"mp4".to_string()));
        let cfg = parse_natural_grabber("get the mp3s from https://example.com depth 1").unwrap();
        assert_eq!(cfg.depth, 1);
        assert_eq!(cfg.extensions, vec!["mp3"]);
    }

    #[test]
    fn whole_site_means_deep_and_same_domain() {
        let cfg =
            parse_natural_grabber("download everything from the whole site https://example.com")
                .unwrap();
        assert_eq!(cfg.depth, 10);
        assert_eq!(cfg.scope, Scope::SameDomain);
        assert!(cfg.extensions.is_empty()); // empty = any non-HTML link
        assert!(cfg.include_embedded);
    }

    #[test]
    fn explicit_extension_and_page_limit() {
        let cfg = parse_natural_grabber(
            "grab the .mkv files from https://example.com, up to 50 pages",
        )
        .unwrap();
        assert_eq!(cfg.extensions, vec!["mkv"]);
        assert_eq!(cfg.max_pages, 50);
    }

    #[test]
    fn exclusion_removes_a_type() {
        let cfg = parse_natural_grabber(
            "download all images from https://example.com but not gifs",
        )
        .unwrap();
        assert!(!cfg.extensions.contains(&"gif".to_string()));
        assert!(cfg.extensions.contains(&"jpg".to_string()));
    }

    #[test]
    fn exclusion_adds_an_exclude_pattern() {
        let cfg = parse_natural_grabber(
            "download all pdfs from https://example.com, skip logout pages",
        )
        .unwrap();
        assert_eq!(cfg.extensions, vec!["pdf"]);
        assert!(cfg.exclude.contains(&"logout".to_string()));
    }

    #[test]
    fn unparseable_input_is_an_error_not_a_guess() {
        assert_eq!(
            parse_natural_grabber("download all the things").unwrap_err(),
            ParseError::MissingUrl
        );
        assert_eq!(
            parse_natural_grabber("grab https://example.com").unwrap_err(),
            ParseError::MissingTarget
        );
        assert_eq!(
            parse_natural_grabber("").unwrap_err(),
            ParseError::MissingUrl
        );
        assert_eq!(
            parse_natural_grabber("download the report.pdf").unwrap_err(),
            ParseError::MissingUrl
        );
    }

    #[test]
    fn scheme_and_trailing_punctuation_are_handled() {
        let cfg =
            parse_natural_grabber("Download all PDFs from https://example.com/docs/.").unwrap();
        assert_eq!(cfg.start_url, "https://example.com/docs/");
        assert_eq!(cfg.extensions, vec!["pdf"]);
    }
}
