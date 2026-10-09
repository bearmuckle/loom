//! Lightweight, dependency-free syntax highlighting and diff classification.
//!
//! The transcript renders code and command output as monospace text. Rather
//! than pull a full tree-sitter grammar set (and its native build) into the
//! client, this module tokenizes the handful of languages an agent transcript
//! actually shows. It is intentionally approximate: the goal is legibility,
//! not parsing.
//!
//! Token spans use byte offsets into the source. Callers pass the spans to
//! [`gpui_kit::StyledText`], which applies them as text runs.

use std::ops::Range;

use gpui_kit::{HighlightStyle, Rgba};

use crate::theme::rgb as theme_rgb;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Json,
    Toml,
    Yaml,
    Bash,
    Sql,
    Go,
    C,
    Cpp,
    Java,
    Ruby,
    Html,
    Css,
    Markdown,
    Diff,
    Text,
}

impl Language {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Bash => "bash",
            Self::Sql => "sql",
            Self::Go => "go",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Java => "java",
            Self::Ruby => "ruby",
            Self::Html => "html",
            Self::Css => "css",
            Self::Markdown => "markdown",
            Self::Diff => "diff",
            Self::Text => "text",
        }
    }

    fn config(self) -> Config {
        match self {
            Self::Rust => Config {
                hash_comment: false,
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
                quote_chars: &['"'],
                keywords: RUST_KEYWORDS,
                types: RUST_TYPES,
                literals: true,
            },
            Self::JavaScript | Self::TypeScript => Config {
                hash_comment: false,
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
                quote_chars: &['"', '\'', '`'],
                keywords: JS_KEYWORDS,
                types: JS_TYPES,
                literals: true,
            },
            Self::Python => Config {
                hash_comment: true,
                line_comment: Some("--"),
                block_comment: None,
                quote_chars: &['"', '\''],
                keywords: PYTHON_KEYWORDS,
                types: PYTHON_TYPES,
                literals: true,
            },
            Self::Ruby => Config {
                hash_comment: true,
                line_comment: None,
                block_comment: Some(("=begin", "=end")),
                quote_chars: &['"', '\''],
                keywords: RUBY_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Bash => Config {
                hash_comment: true,
                line_comment: None,
                block_comment: None,
                quote_chars: &['"', '\''],
                keywords: BASH_KEYWORDS,
                types: &[],
                literals: false,
            },
            Self::Toml => Config {
                hash_comment: true,
                line_comment: None,
                block_comment: None,
                quote_chars: &['"', '\''],
                keywords: TOML_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Yaml => Config {
                hash_comment: true,
                line_comment: None,
                block_comment: None,
                quote_chars: &['"', '\''],
                keywords: YAML_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Sql => Config {
                hash_comment: false,
                line_comment: Some("--"),
                block_comment: Some(("/*", "*/")),
                quote_chars: &['\'', '"'],
                keywords: SQL_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Go | Self::C | Self::Cpp | Self::Java => Config {
                hash_comment: false,
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
                quote_chars: &['"', '\''],
                keywords: C_KEYWORDS,
                types: C_TYPES,
                literals: true,
            },
            Self::Html => Config {
                hash_comment: false,
                line_comment: None,
                block_comment: Some(("<!--", "-->")),
                quote_chars: &['"', '\''],
                keywords: &[],
                types: &[],
                literals: false,
            },
            Self::Css => Config {
                hash_comment: false,
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
                quote_chars: &['"', '\''],
                keywords: CSS_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Json => Config {
                hash_comment: false,
                line_comment: None,
                block_comment: None,
                quote_chars: &['"'],
                keywords: JSON_KEYWORDS,
                types: &[],
                literals: true,
            },
            Self::Markdown => Config {
                hash_comment: false,
                line_comment: None,
                block_comment: None,
                quote_chars: &['`'],
                keywords: &[],
                types: &[],
                literals: false,
            },
            Self::Diff | Self::Text => Config::plain(),
        }
    }

    /// Pick a language from a file path or a bare extension. Falls back to
    /// [`Language::Text`] for anything unrecognized.
    pub(crate) fn from_path(path: &str) -> Self {
        let trimmed = path
            .trim()
            .trim_matches(|character| character == '"' || character == '\'');
        let extension = trimmed
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension)
            .unwrap_or(trimmed);
        Self::from_extension(extension)
    }

    /// Pick a language from a tool-provided hint such as `rust` or `py`.
    pub(crate) fn from_hint(hint: &str) -> Self {
        match hint.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Self::Rust,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "typescript" | "ts" | "tsx" => Self::TypeScript,
            "python" | "py" => Self::Python,
            "json" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "bash" | "sh" | "shell" | "zsh" => Self::Bash,
            "sql" => Self::Sql,
            "go" => Self::Go,
            "c" => Self::C,
            "cpp" | "c++" | "cc" | "hpp" => Self::Cpp,
            "java" => Self::Java,
            "ruby" | "rb" => Self::Ruby,
            "html" | "htm" => Self::Html,
            "css" => Self::Css,
            "markdown" | "md" => Self::Markdown,
            "diff" | "patch" => Self::Diff,
            other => Self::from_extension(other),
        }
    }

    fn from_extension(extension: &str) -> Self {
        match extension.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "py" | "pyi" => Self::Python,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sh" | "bash" | "zsh" | "fish" => Self::Bash,
            "sql" => Self::Sql,
            "go" => Self::Go,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" => Self::Cpp,
            "java" => Self::Java,
            "rb" => Self::Ruby,
            "html" | "htm" => Self::Html,
            "css" => Self::Css,
            "md" | "markdown" => Self::Markdown,
            "diff" | "patch" => Self::Diff,
            _ => Self::Text,
        }
    }
}

struct Config {
    hash_comment: bool,
    line_comment: Option<&'static str>,
    block_comment: Option<(&'static str, &'static str)>,
    quote_chars: &'static [char],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    literals: bool,
}

impl Config {
    const fn plain() -> Self {
        Self {
            hash_comment: false,
            line_comment: None,
            block_comment: None,
            quote_chars: &[],
            keywords: &[],
            types: &[],
            literals: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TokenKind {
    Keyword,
    Literal,
    String,
    Comment,
    Number,
    Function,
    Type,
    Property,
    Plain,
}

/// A single highlighted span, as a byte range into the source.
pub(crate) type Span = (Range<usize>, TokenKind);

/// Tokenizes `code` into highlight spans. `Plain` text is intentionally
/// omitted so callers can treat the gaps as the default style.
pub(crate) fn highlight(code: &str, language: Language) -> Vec<Span> {
    let config = language.config();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < code.len() {
        let rest = &code[index..];

        if config.hash_comment && rest.starts_with('#') {
            let end = line_end(code, index);
            spans.push((index..end, TokenKind::Comment));
            index = end;
            continue;
        }
        if let Some(prefix) = config.line_comment
            && rest.starts_with(prefix)
        {
            let end = line_end(code, index);
            spans.push((index..end, TokenKind::Comment));
            index = end;
            continue;
        }
        if let Some((open, close)) = config.block_comment
            && rest.starts_with(open)
        {
            let end = rest
                .find(close)
                .map_or(code.len(), |offset| index + offset + close.len());
            spans.push((index..end, TokenKind::Comment));
            index = end;
            continue;
        }

        let character = rest.chars().next().expect("index is on a char boundary");
        if config.quote_chars.contains(&character) {
            let end = string_end(code, index, character);
            spans.push((index..end, TokenKind::String));
            index = end;
            continue;
        }
        if character.is_ascii_digit() {
            let end = number_end(code, index);
            spans.push((index..end, TokenKind::Number));
            index = end;
            continue;
        }
        if is_identifier_start(character) {
            let end = identifier_end(code, index);
            let word = &code[index..end];
            let kind = classify_word(word, &config, code, end);
            if kind != TokenKind::Plain {
                spans.push((index..end, kind));
            }
            index = end;
            continue;
        }

        index += character.len_utf8();
    }
    spans
}

fn classify_word(word: &str, config: &Config, code: &str, end: usize) -> TokenKind {
    if config.literals && matches!(word, "true" | "false" | "null" | "nil" | "None") {
        return TokenKind::Literal;
    }
    if config.keywords.contains(&word) {
        return TokenKind::Keyword;
    }
    if config.types.contains(&word) || word.chars().next().is_some_and(char::is_uppercase) {
        return TokenKind::Type;
    }
    if next_non_space(code, end) == Some('(') {
        return TokenKind::Function;
    }
    if next_non_space(code, end) == Some(':') && !code[end..].starts_with("::") {
        return TokenKind::Property;
    }
    TokenKind::Plain
}

fn next_non_space(code: &str, index: usize) -> Option<char> {
    code.get(index..)?
        .chars()
        .find(|character| !character.is_whitespace())
}

fn line_end(code: &str, index: usize) -> usize {
    code[index..]
        .find('\n')
        .map_or(code.len(), |offset| index + offset)
}

fn string_end(code: &str, index: usize, quote: char) -> usize {
    let mut characters = code[index + quote.len_utf8()..].char_indices();
    while let Some((offset, character)) = characters.next() {
        let position = index + quote.len_utf8() + offset;
        if character == '\\' {
            characters.next();
            continue;
        }
        if character == quote {
            return position + quote.len_utf8();
        }
        if character == '\n' && quote != '`' {
            return position;
        }
    }
    code.len()
}

fn number_end(code: &str, index: usize) -> usize {
    let mut end = index;
    for (offset, character) in code[index..].char_indices() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | 'x' | 'X') {
            end = index + offset + character.len_utf8();
        } else {
            break;
        }
    }
    end.max(index + 1)
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphabetic()
}

fn identifier_end(code: &str, index: usize) -> usize {
    let mut end = index;
    for (offset, character) in code[index..].char_indices() {
        if character.is_alphanumeric() || matches!(character, '_' | '$' | '-') {
            end = index + offset + character.len_utf8();
        } else {
            break;
        }
    }
    end.max(index + 1)
}

/// Maps a token kind to its theme color.
pub(crate) fn token_color(kind: TokenKind) -> Rgba {
    match kind {
        TokenKind::Keyword => theme_rgb(0xc4b5fd),
        TokenKind::Literal => theme_rgb(0xfbbf24),
        TokenKind::String => theme_rgb(0x86efac),
        TokenKind::Comment => theme_rgb(0x64748b),
        TokenKind::Number => theme_rgb(0xfbbf24),
        TokenKind::Function => theme_rgb(0x93c5fd),
        TokenKind::Type => theme_rgb(0xbfdbfe),
        TokenKind::Property => theme_rgb(0x60a5fa),
        TokenKind::Plain => theme_rgb(0xdbeafe),
    }
}

/// Builds GPUI highlight styles for the spans that fall within a single line.
pub(crate) fn line_highlights(
    spans: &[Span],
    line: Range<usize>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    spans
        .iter()
        .filter(|(span, _)| span.start < line.end && span.end > line.start)
        .map(|(span, kind)| {
            let start = span.start.max(line.start) - line.start;
            let end = span.end.min(line.end) - line.start;
            (
                start..end,
                HighlightStyle {
                    color: Some(token_color(*kind).into()),
                    ..Default::default()
                },
            )
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffLineKind {
    Added,
    Removed,
    Context,
    Hunk,
    Meta,
}

/// Classifies one line of a unified diff.
pub(crate) fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("@@") {
        DiffLineKind::Hunk
    } else if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
    {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Added
    } else if line.starts_with('-') {
        DiffLineKind::Removed
    } else {
        DiffLineKind::Context
    }
}

/// The surface and foreground colors for one diff line.
pub(crate) fn diff_line_style(kind: DiffLineKind) -> (Option<Rgba>, Rgba) {
    match kind {
        DiffLineKind::Added => (Some(theme_rgb(0x24543d)), theme_rgb(0xbbf7d0)),
        DiffLineKind::Removed => (Some(theme_rgb(0x542936)), theme_rgb(0xfecaca)),
        DiffLineKind::Context => (None, theme_rgb(0xcbd5e1)),
        DiffLineKind::Hunk => (Some(theme_rgb(0x293244)), theme_rgb(0x93c5fd)),
        DiffLineKind::Meta => (None, theme_rgb(0x64748b)),
    }
}

/// Whether text looks like a unified diff or `git diff` patch.
pub(crate) fn looks_like_patch(text: &str) -> bool {
    text.lines().take(40).any(|line| {
        line.starts_with("@@")
            || line.starts_with("diff --git")
            || line.starts_with("--- a/")
            || line.starts_with("+++ b/")
    })
}

/// A compact, human-readable label for a patch, e.g. `+14 -3`.
pub(crate) fn patch_summary(text: &str) -> Option<String> {
    if !looks_like_patch(text) {
        return None;
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in text.lines() {
        match classify_diff_line(line) {
            DiffLineKind::Added => added += 1,
            DiffLineKind::Removed => removed += 1,
            _ => {}
        }
    }
    Some(format!("+{added} -{removed}"))
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while", "union",
];

const RUST_TYPES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "String",
    "u8", "u16", "u32", "u64", "u128", "usize", "Vec", "Option", "Result", "Box", "Rc", "Arc",
    "HashMap", "BTreeMap", "HashSet", "BTreeSet", "Some", "None", "Ok", "Err",
];

const JS_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "yield",
];

const JS_TYPES: &[&str] = &[
    "any",
    "boolean",
    "interface",
    "never",
    "number",
    "object",
    "string",
    "symbol",
    "type",
    "undefined",
    "unknown",
    "void",
    "Array",
    "Promise",
    "Record",
    "Map",
    "Set",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

const PYTHON_TYPES: &[&str] = &[
    "bool",
    "bytes",
    "dict",
    "float",
    "frozenset",
    "int",
    "list",
    "object",
    "set",
    "str",
    "tuple",
    "type",
    "self",
    "cls",
];

const RUBY_KEYWORDS: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "do", "else", "elsif", "end",
    "ensure", "for", "if", "in", "module", "next", "not", "or", "redo", "rescue", "retry",
    "return", "then", "unless", "until", "when", "while", "yield",
];

const BASH_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in", "select",
    "then", "until", "while", "export", "local", "readonly", "return", "echo", "cd", "set",
];

const TOML_KEYWORDS: &[&str] = &["true", "false"];

const YAML_KEYWORDS: &[&str] = &["true", "false", "null", "yes", "no"];

const SQL_KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "insert",
    "into",
    "update",
    "delete",
    "create",
    "table",
    "drop",
    "alter",
    "index",
    "join",
    "inner",
    "left",
    "right",
    "outer",
    "on",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "as",
    "and",
    "or",
    "not",
    "null",
    "values",
    "set",
    "distinct",
    "union",
    "all",
    "case",
    "when",
    "then",
    "else",
    "end",
    "primary",
    "key",
    "foreign",
    "references",
    "select",
];

const C_KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "else",
    "enum",
    "extends",
    "extern",
    "final",
    "finally",
    "for",
    "function",
    "goto",
    "if",
    "implements",
    "import",
    "inline",
    "interface",
    "namespace",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "throws",
    "try",
    "typedef",
    "typename",
    "union",
    "using",
    "virtual",
    "volatile",
    "while",
];

const C_TYPES: &[&str] = &[
    "bool", "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void",
    "size_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t",
    "uint64_t", "string", "auto",
];

const CSS_KEYWORDS: &[&str] = &[
    "important",
    "media",
    "import",
    "keyframes",
    "supports",
    "font-face",
    "root",
    "from",
    "to",
];

const JSON_KEYWORDS: &[&str] = &["true", "false", "null"];

#[cfg(test)]
mod tests {
    use super::{
        DiffLineKind, Language, Span, TokenKind, classify_diff_line, highlight, line_highlights,
        looks_like_patch, patch_summary,
    };

    fn kinds(code: &str, language: Language) -> Vec<TokenKind> {
        highlight(code, language)
            .into_iter()
            .map(|(_, kind)| kind)
            .collect()
    }

    #[test]
    fn every_file_extension_and_hint_maps_to_a_language() {
        for (path, expected) in [
            ("src/main.rs", Language::Rust),
            ("app.tsx", Language::TypeScript),
            ("index.mjs", Language::JavaScript),
            ("script.py", Language::Python),
            ("Cargo.toml", Language::Toml),
            ("config.yml", Language::Yaml),
            ("run.sh", Language::Bash),
            ("query.sql", Language::Sql),
            ("main.go", Language::Go),
            ("lib.rs", Language::Rust),
            ("Makefile", Language::Text),
        ] {
            assert_eq!(Language::from_path(path), expected, "path {path}");
        }
        assert_eq!(Language::from_hint("rust"), Language::Rust);
        assert_eq!(Language::from_hint("PY"), Language::Python);
        assert_eq!(Language::from_hint("ts"), Language::TypeScript);
        assert_eq!(Language::from_hint(""), Language::Text);
        assert_eq!(Language::from_hint("klingon"), Language::Text);
        for language in [
            Language::Rust,
            Language::JavaScript,
            Language::Python,
            Language::Json,
            Language::Text,
        ] {
            assert!(!language.label().is_empty());
        }
    }

    #[test]
    fn rust_keywords_strings_and_comments_are_classified() {
        let found = kinds("let x = \"hi\"; // note", Language::Rust);
        assert!(found.contains(&TokenKind::Keyword));
        assert!(found.contains(&TokenKind::String));
        assert!(found.contains(&TokenKind::Comment));
    }

    #[test]
    fn block_comments_span_lines_and_numbers_are_found() {
        let spans = highlight("/* a\n b */\n42", Language::Rust);
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Comment));
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Number));
    }

    #[test]
    fn unterminated_strings_and_comments_stop_at_the_end() {
        assert_eq!(
            kinds("\"unterminated", Language::Rust),
            vec![TokenKind::String]
        );
        assert_eq!(
            kinds("# trailing", Language::Python),
            vec![TokenKind::Comment]
        );
        assert_eq!(kinds("/* open", Language::Rust), vec![TokenKind::Comment]);
    }

    #[test]
    fn identifiers_classify_function_property_and_type() {
        let spans = highlight("foo(String: 1)", Language::Rust);
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Function));
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Type));
        let spans = highlight("key: value", Language::Yaml);
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Property));
    }

    #[test]
    fn unicode_identifiers_do_not_split_char_boundaries() {
        let code = "let café = \"héllo\"";
        let spans = highlight(code, Language::Rust);
        for (range, _) in &spans {
            assert!(code.is_char_boundary(range.start));
            assert!(code.is_char_boundary(range.end));
        }
    }

    #[test]
    fn line_highlights_are_relative_to_the_line() {
        let code = "let x";
        let spans: Vec<Span> = vec![(0..3, TokenKind::Keyword)];
        let highlights = line_highlights(&spans, 0..code.len());
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].0, 0..3);
    }

    #[test]
    fn diff_lines_are_classified_and_summarized() {
        let patch = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n- old\n+ new\n context";
        assert_eq!(classify_diff_line("@@ -1 +1 @@"), DiffLineKind::Hunk);
        assert_eq!(classify_diff_line("diff --git a b"), DiffLineKind::Meta);
        assert_eq!(classify_diff_line("+add"), DiffLineKind::Added);
        assert_eq!(classify_diff_line("-del"), DiffLineKind::Removed);
        assert_eq!(classify_diff_line(" ctx"), DiffLineKind::Context);
        assert!(looks_like_patch(patch));
        assert_eq!(patch_summary(patch).as_deref(), Some("+1 -1"));
        assert_eq!(patch_summary("hello"), None);
    }
}
