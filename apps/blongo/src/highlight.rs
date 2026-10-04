//! Syntax highlighting of fenced code blocks with tree-sitter (MIT grammars).
//!
//! Lazy on two levels: a language's highlight configuration (query
//! compilation) is built the first time a block in that language needs it,
//! and a block is highlighted only when it is first rendered, on a
//! background thread (see `timeline.rs`). Until then it renders plain.

use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Highlight classes, in the order of [`NAMES`]. A capture like
/// `function.method` resolves to its longest listed prefix (`function`).
pub const NAMES: &[&str] = &[
    "comment",
    "keyword",
    "string",
    "number",
    "constant",
    "function",
    "type",
    "property",
    "attribute",
    "operator",
    "variable.builtin",
    "constructor",
    "tag",
    "escape",
    "label",
    "boolean",
    "module",
];

/// `(byte range, class index into NAMES)`, sorted and non-overlapping.
pub type Spans = Vec<(Range<usize>, u8)>;

/// Above this a block stays plain: highlighting must stay cheap.
pub const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Bash,
    Json,
    Go,
    C,
}

impl Lang {
    /// From a fence's info string (`rust`, `py`, `ts`, `sh`, …).
    pub fn from_info(info: &str) -> Option<Self> {
        let tag = info
            .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Some(match tag.as_str() {
            "rust" | "rs" => Self::Rust,
            "python" | "py" | "python3" => Self::Python,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" | "node" => Self::JavaScript,
            "typescript" | "ts" | "tsx" | "mts" => Self::TypeScript,
            "bash" | "sh" | "shell" | "zsh" | "console" | "shellscript" => Self::Bash,
            "json" | "jsonc" | "json5" => Self::Json,
            "go" | "golang" => Self::Go,
            "c" | "h" | "cpp" | "c++" | "cc" | "hpp" => Self::C,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }

    fn build(self) -> Option<HighlightConfiguration> {
        let ts_query;
        let (language, highlights, injections, locals) = match self {
            Self::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::Python => (
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                "",
                tree_sitter_javascript::LOCALS_QUERY,
            ),
            Self::TypeScript => {
                // The TypeScript query extends the JavaScript one.
                ts_query = format!(
                    "{}\n{}",
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    tree_sitter_javascript::HIGHLIGHT_QUERY
                );
                (
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    ts_query.as_str(),
                    "",
                    tree_sitter_typescript::LOCALS_QUERY,
                )
            }
            Self::Bash => (
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Self::Json => (
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::Go => (
                tree_sitter_go::LANGUAGE.into(),
                tree_sitter_go::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::C => (
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY,
                "",
                "",
            ),
        };
        let mut config =
            HighlightConfiguration::new(language, "code", highlights, injections, locals).ok()?;
        config.configure(NAMES);
        Some(config)
    }
}

const LANGS: usize = 8;

fn config(lang: Lang) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; LANGS] =
        [const { OnceLock::new() }; LANGS];
    CONFIGS[lang.index()].get_or_init(|| lang.build()).as_ref()
}

/// Non-overlapping `(byte range, class index into NAMES)` spans of `code`.
/// Empty when the language has no usable configuration or the code is too
/// large.
pub fn highlight(lang: Lang, code: &str) -> Spans {
    // Profiling switch (tools/profile.py A/B runs): leave code plain.
    static DISABLED: OnceLock<bool> = OnceLock::new();
    if *DISABLED.get_or_init(|| std::env::var_os("BLONGO_NO_HIGHLIGHT").is_some()) {
        return Vec::new();
    }
    let Some(config) = config(lang).filter(|_| code.len() <= MAX_BYTES) else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(config, code.as_bytes(), None, |_| None) else {
        return Vec::new();
    };
    let mut spans: Spans = Vec::new();
    let mut stack: Vec<u8> = Vec::new();
    for event in events {
        match event {
            Ok(HighlightEvent::HighlightStart(h)) => stack.push(h.0 as u8),
            Ok(HighlightEvent::HighlightEnd) => {
                stack.pop();
            }
            Ok(HighlightEvent::Source { start, end }) => {
                if let Some(&class) = stack.last()
                    && start < end
                {
                    // Merge with an adjacent span of the same class.
                    match spans.last_mut() {
                        Some((range, c)) if *c == class && range.end == start => range.end = end,
                        _ => spans.push((start..end, class)),
                    }
                }
            }
            Err(_) => return Vec::new(),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of<'a>(code: &'a str, spans: &[(Range<usize>, u8)], word: &str) -> Option<&'a str> {
        let at = code.find(word)?;
        spans
            .iter()
            .find(|(r, _)| r.start <= at && at < r.end)
            .map(|(_, c)| NAMES[*c as usize])
    }

    #[test]
    fn fence_info_maps_to_languages() {
        assert_eq!(Lang::from_info("rust"), Some(Lang::Rust));
        assert_eq!(Lang::from_info("ts title=\"x\""), Some(Lang::TypeScript));
        assert_eq!(Lang::from_info("Python"), Some(Lang::Python));
        assert_eq!(Lang::from_info("brainfuck"), None);
        assert_eq!(Lang::from_info(""), None);
    }

    #[test]
    fn every_language_highlights() {
        let samples = [
            (Lang::Rust, "fn main() { let x = \"hi\"; }", "fn", "keyword"),
            (
                Lang::Python,
                "def f():\n    return 'a'  # c",
                "def",
                "keyword",
            ),
            (Lang::JavaScript, "const x = 'a'; // c", "const", "keyword"),
            (Lang::TypeScript, "const x: number = 1;", "const", "keyword"),
            (Lang::Bash, "echo \"hi\" # c", "# c", "comment"),
            (Lang::Json, "{\"a\": 1}", "1", "number"),
            (Lang::Go, "package main\nfunc f() {}", "func", "keyword"),
            (Lang::C, "int main(void) { return 0; }", "return", "keyword"),
        ];
        for (lang, code, word, want) in samples {
            let spans = highlight(lang, code);
            assert!(!spans.is_empty(), "{lang:?}");
            assert_eq!(
                class_of(code, &spans, word),
                Some(want),
                "{lang:?}: {spans:?}"
            );
            // Sorted, non-overlapping, in bounds.
            for pair in spans.windows(2) {
                assert!(pair[0].0.end <= pair[1].0.start, "{lang:?}");
            }
            assert!(spans.last().unwrap().0.end <= code.len());
        }
    }

    #[test]
    fn oversized_code_stays_plain() {
        let code = "let x = 1;\n".repeat(MAX_BYTES / 10);
        assert!(highlight(Lang::Rust, &code).is_empty());
    }
}
