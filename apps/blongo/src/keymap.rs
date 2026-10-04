//! Commands, their key bindings and when-clauses.
//!
//! Every shell command has a stable id (`thread.new`, `palette.files`, …)
//! listed in [`COMMANDS`]; the command palette shows the same list. Keys
//! are bound to a [`Cmd`] action that carries the id and an optional
//! when-clause; the shell evaluates the clause against its state when the
//! key is pressed and lets the key fall through to the next binding when it
//! is false.
//!
//! The defaults can be changed in `keybindings.json` in the config dir:
//!
//! ```json
//! [
//!   { "key": "ctrl-k", "command": "palette.commands" },
//!   { "key": "alt-d", "command": "view.diff", "when": "threadOpen && !busy" },
//!   { "key": "alt-f", "command": "-thread.fork" }
//! ]
//! ```
//!
//! A command prefixed with `-` removes its default binding (for that key,
//! or every key when `key` is left out). User bindings win over defaults.
//!
//! When-clauses: context names joined with `!`, `&&`, `||` and parentheses;
//! an unknown name is false. Names: `threadOpen`, `busy`, `remote`,
//! `terminalOpen`, `paletteOpen`, `view.chat`, `view.diff`, `view.files`,
//! `view.inbox`, `view.settings`.

use std::collections::HashSet;
use std::path::Path;

use gpui::{App, KeyBinding, SharedString};
use serde::Deserialize;

pub const CONTEXT: &str = "Shell";

/// A key binding's command, with the clause that must hold.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = shell, no_json)]
pub struct Cmd {
    pub id: SharedString,
    pub when: Option<When>,
}

pub struct CommandDef {
    pub id: &'static str,
    pub title: &'static str,
    pub key: Option<&'static str>,
    pub when: Option<&'static str>,
}

const fn c(
    id: &'static str,
    title: &'static str,
    key: Option<&'static str>,
    when: Option<&'static str>,
) -> CommandDef {
    CommandDef {
        id,
        title,
        key,
        when,
    }
}

pub const COMMANDS: &[CommandDef] = &[
    c(
        "palette.commands",
        "Show all commands",
        Some("secondary-shift-p"),
        None,
    ),
    c(
        "palette.files",
        "Go to file…",
        Some("secondary-p"),
        Some("threadOpen"),
    ),
    c("thread.new", "New thread", Some("secondary-n"), None),
    c(
        "thread.fork",
        "Fork this thread",
        Some("alt-f"),
        Some("threadOpen"),
    ),
    c(
        "thread.undo",
        "Undo the last turn",
        Some("alt-z"),
        Some("threadOpen"),
    ),
    c("thread.stop", "Stop the running turn", None, Some("busy")),
    c(
        "terminal.toggle",
        "Toggle the terminal",
        Some("secondary-`"),
        Some("threadOpen"),
    ),
    c(
        "view.chat",
        "Show the conversation",
        Some("alt-c"),
        Some("threadOpen"),
    ),
    c(
        "view.diff",
        "Show this thread's changes",
        Some("alt-d"),
        Some("threadOpen"),
    ),
    c(
        "view.files",
        "Browse files",
        Some("alt-e"),
        Some("threadOpen"),
    ),
    c("view.inbox", "Open the review inbox", Some("alt-i"), None),
    c("view.settings", "Open settings", Some("secondary-,"), None),
    c("provider.codex", "Use Codex", Some("alt-1"), None),
    c(
        "provider.claude-code",
        "Use Claude Code",
        Some("alt-2"),
        None,
    ),
    c(
        "provider.antigravity",
        "Use Antigravity",
        Some("alt-3"),
        None,
    ),
    c("provider.acp", "Use the ACP agent", Some("alt-4"), None),
    c(
        "model.next",
        "Next model",
        Some("alt-m"),
        Some("threadOpen"),
    ),
    c("theme.toggle", "Toggle light / dark theme", None, None),
    c("approval.toggle", "Toggle auto-approve", None, None),
    c(
        "git.refresh",
        "Refresh git status",
        None,
        Some("view.files"),
    ),
    c(
        "keybindings.open",
        "Show the keybindings file location",
        None,
        None,
    ),
];

pub fn command(id: &str) -> Option<&'static CommandDef> {
    COMMANDS.iter().find(|c| c.id == id)
}

// ------------------------------------------------------------ when-clauses

/// A parsed when-clause.
#[derive(Clone, Debug, PartialEq)]
pub enum When {
    Name(SharedString),
    Not(Box<When>),
    And(Box<When>, Box<When>),
    Or(Box<When>, Box<When>),
}

/// Longest when-clause accepted.
const MAX_WHEN_LEN: usize = 512;
/// Deepest nesting of `(` and `!` in a when-clause (the parser recurses).
const MAX_WHEN_DEPTH: usize = 32;

impl When {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_WHEN_LEN {
            return Err(format!(
                "when-clauses are limited to {MAX_WHEN_LEN} characters"
            ));
        }
        let tokens = tokenize(text)?;
        let mut p = Parser {
            tokens,
            at: 0,
            depth: 0,
        };
        let expr = p.or()?;
        if p.at != p.tokens.len() {
            return Err(format!("unexpected `{}` in `{text}`", p.tokens[p.at]));
        }
        Ok(expr)
    }

    pub fn eval(&self, names: &HashSet<&str>) -> bool {
        match self {
            Self::Name(n) => names.contains(n.as_ref()),
            Self::Not(e) => !e.eval(names),
            Self::And(a, b) => a.eval(names) && b.eval(names),
            Self::Or(a, b) => a.eval(names) || b.eval(names),
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '!' | '(' | ')' => {
                out.push(c.to_string());
                i += 1;
            }
            '&' | '|' => {
                if chars.get(i + 1) != Some(&c) {
                    return Err(format!("use `{c}{c}` in `{text}`"));
                }
                out.push(format!("{c}{c}"));
                i += 2;
            }
            c if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_' | '-'))
                {
                    i += 1;
                }
                out.push(chars[start..i].iter().collect());
            }
            other => return Err(format!("unexpected `{other}` in `{text}`")),
        }
    }
    if out.is_empty() {
        return Err("empty when-clause".into());
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<String>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn or(&mut self) -> Result<When, String> {
        let mut left = self.and()?;
        while self.peek() == Some("||") {
            self.at += 1;
            left = When::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<When, String> {
        let mut left = self.unary()?;
        while self.peek() == Some("&&") {
            self.at += 1;
            left = When::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<When, String> {
        self.depth += 1;
        let result = self.unary_inner();
        self.depth -= 1;
        result
    }

    fn unary_inner(&mut self) -> Result<When, String> {
        if self.depth > MAX_WHEN_DEPTH {
            return Err(format!("when-clauses nest at most {MAX_WHEN_DEPTH} deep"));
        }
        match self.peek() {
            Some("!") => {
                self.at += 1;
                Ok(When::Not(Box::new(self.unary()?)))
            }
            Some("(") => {
                self.at += 1;
                let e = self.or()?;
                if self.peek() != Some(")") {
                    return Err("missing `)`".into());
                }
                self.at += 1;
                Ok(e)
            }
            Some(t) if !matches!(t, ")" | "&&" | "||") => {
                let name = t.to_owned();
                self.at += 1;
                Ok(When::Name(name.into()))
            }
            Some(t) => Err(format!("unexpected `{t}`")),
            None => Err("the clause ends too early".into()),
        }
    }
}

// --------------------------------------------------------------- bindings

#[derive(Debug, Deserialize)]
struct UserBinding {
    #[serde(default)]
    key: Option<String>,
    command: String,
    #[serde(default)]
    when: Option<String>,
}

/// A binding as it ends up in the keymap.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub key: String,
    pub command: &'static str,
    pub when: Option<When>,
}

fn valid_key(key: &str) -> Result<(), String> {
    if key.split_whitespace().next().is_none() {
        return Err("empty key".into());
    }
    for part in key.split_whitespace() {
        gpui::Keystroke::parse(part).map_err(|e| format!("`{key}`: {e}"))?;
    }
    Ok(())
}

/// Defaults overlaid with the user's file (`text`: its contents, if any).
/// Problems are reported, and the bindings they concern skipped.
pub fn resolve(text: Option<&str>) -> (Vec<Binding>, Vec<String>) {
    let mut problems = Vec::new();
    let mut bindings: Vec<Binding> = COMMANDS
        .iter()
        .filter_map(|c| {
            Some(Binding {
                key: c.key?.to_owned(),
                command: c.id,
                when: c
                    .when
                    .map(|w| When::parse(w).expect("default when-clauses parse")),
            })
        })
        .collect();
    let Some(text) = text else {
        return (bindings, problems);
    };
    let user: Vec<UserBinding> = match serde_json::from_str(text) {
        Ok(u) => u,
        Err(e) => {
            problems.push(format!("keybindings.json is not valid: {e}"));
            return (bindings, problems);
        }
    };
    for b in user {
        if let Some(id) = b.command.strip_prefix('-') {
            let before = bindings.len();
            bindings.retain(|x| !(x.command == id && b.key.as_ref().is_none_or(|k| *k == x.key)));
            if before == bindings.len() {
                problems.push(format!("nothing to remove for `{}`", b.command));
            }
            continue;
        }
        let Some(def) = command(&b.command) else {
            problems.push(format!("unknown command `{}`", b.command));
            continue;
        };
        let Some(key) = b.key else {
            problems.push(format!("`{}` has no key", b.command));
            continue;
        };
        if let Err(e) = valid_key(&key) {
            problems.push(e);
            continue;
        }
        let when = match b.when.as_deref().map(When::parse).transpose() {
            Ok(w) => w,
            Err(e) => {
                problems.push(format!("`{}`: {e}", b.command));
                continue;
            }
        };
        bindings.push(Binding {
            key,
            command: def.id,
            when,
        });
    }
    (bindings, problems)
}

/// Install the shell's bindings (replacing earlier ones of every context:
/// the caller re-adds the others' first). Returns the problems found in the
/// user's file.
pub fn install(path: &Path, cx: &mut App) -> (Vec<Binding>, Vec<String>) {
    let text = std::fs::read_to_string(path).ok();
    let (bindings, problems) = resolve(text.as_deref());
    cx.bind_keys(bindings.iter().map(|b| {
        KeyBinding::new(
            &b.key,
            Cmd {
                id: b.command.into(),
                when: b.when.clone(),
            },
            Some(CONTEXT),
        )
    }));
    (bindings, problems)
}

/// The key shown next to a command in the palette (the last binding wins,
/// like the keymap).
pub fn key_for<'a>(bindings: &'a [Binding], id: &str) -> Option<&'a str> {
    bindings
        .iter()
        .rev()
        .find(|b| b.command == id)
        .map(|b| b.key.as_str())
}

/// A key as people read it: `secondary-shift-p` → `Ctrl+Shift+P` (`Cmd`
/// on macOS).
pub fn display(key: &str) -> String {
    key.split_whitespace()
        .map(|stroke| {
            let parts: Vec<&str> = stroke.split('-').collect();
            let n = parts.len();
            parts
                .iter()
                .enumerate()
                .map(|(i, p)| match *p {
                    "secondary" if cfg!(target_os = "macos") => "Cmd".to_owned(),
                    "secondary" | "ctrl" => "Ctrl".to_owned(),
                    "cmd" | "super" | "win" => "Cmd".to_owned(),
                    "alt" => "Alt".to_owned(),
                    "shift" => "Shift".to_owned(),
                    "" if i + 1 == n => "-".to_owned(),
                    p if i + 1 == n && p.chars().count() == 1 => p.to_uppercase(),
                    p => {
                        let mut c = p.chars();
                        c.next()
                            .map(|f| f.to_uppercase().chain(c).collect())
                            .unwrap_or_default()
                    }
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_display_readably() {
        if !cfg!(target_os = "macos") {
            assert_eq!(display("secondary-shift-p"), "Ctrl+Shift+P");
            assert_eq!(display("secondary-,"), "Ctrl+,");
        }
        assert_eq!(display("alt-1"), "Alt+1");
        assert_eq!(display("ctrl-k ctrl-s"), "Ctrl+K Ctrl+S");
        assert_eq!(display("alt-enter"), "Alt+Enter");
    }

    #[test]
    fn when_clauses_parse_and_evaluate() {
        let names: HashSet<&str> = ["threadOpen", "view.diff"].into_iter().collect();
        let t = |s: &str| When::parse(s).unwrap().eval(&names);
        assert!(t("threadOpen"));
        assert!(!t("busy"));
        assert!(t("threadOpen && !busy"));
        assert!(t("busy || view.diff"));
        // Deep nesting and huge clauses are refused, not recursed into.
        assert!(When::parse(&format!("{}a{}", "(".repeat(200), ")".repeat(200))).is_err());
        assert!(When::parse(&"!".repeat(100).to_string()).is_err());
        assert!(When::parse(&format!("{}a{}", "(".repeat(10), ")".repeat(10))).is_ok());
        assert!(When::parse(&vec!["a"; 400].join(" && ")).is_err());
        assert!(!t("!(threadOpen && view.diff)"));
        assert!(t("busy || threadOpen && view.diff"));
        assert!(!t("(busy || threadOpen) && remote"));
        assert!(When::parse("a &").is_err());
        assert!(When::parse("a && ").is_err());
        assert!(When::parse("(a").is_err());
        assert!(When::parse("a b").is_err());
        assert!(When::parse("").is_err());
        assert!(When::parse("a == b").is_err());
    }

    #[test]
    fn defaults_are_valid() {
        let (bindings, problems) = resolve(None);
        assert!(problems.is_empty());
        for b in &bindings {
            valid_key(&b.key).unwrap();
        }
        let ids: HashSet<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), COMMANDS.len(), "command ids are unique");
    }

    #[test]
    fn user_file_adds_removes_and_reports() {
        let file = r#"[
            {"key": "ctrl-k", "command": "palette.commands"},
            {"key": "alt-f", "command": "-thread.fork"},
            {"command": "-view.inbox"},
            {"key": "alt-x", "command": "view.diff", "when": "threadOpen && !busy"},
            {"key": "alt-y", "command": "no.such"},
            {"key": "alt-q", "command": "view.chat", "when": "a &&"},
            {"key": "k-ctrl", "command": "view.chat"},
            {"command": "-thread.fork"}
        ]"#;
        let (bindings, problems) = resolve(Some(file));
        assert_eq!(key_for(&bindings, "palette.commands"), Some("ctrl-k"));
        assert_eq!(key_for(&bindings, "thread.fork"), None);
        assert_eq!(key_for(&bindings, "view.inbox"), None);
        let diff = bindings
            .iter()
            .rev()
            .find(|b| b.command == "view.diff")
            .unwrap();
        assert_eq!(diff.key, "alt-x");
        assert!(diff.when.is_some());
        // The default alt-d stays as well.
        assert!(
            bindings
                .iter()
                .any(|b| b.command == "view.diff" && b.key == "alt-d")
        );
        assert_eq!(problems.len(), 4, "{problems:?}");
        let (bindings, problems) = resolve(Some("{oops"));
        assert_eq!(bindings, resolve(None).0);
        assert_eq!(problems.len(), 1);
    }
}
