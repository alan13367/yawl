//! Small syntax highlighter for fenced code blocks and diff views.
//!
//! A [`Highlighter`] colors one line at a time and carries only what must
//! survive a line break: an open block comment or multi-line string. It
//! knows a handful of token classes per language (comments, strings,
//! numbers, keywords, calls, types, keys, and shell commands, flags, and
//! variables) and renders unknown languages as escaped plain text.

use super::theme;

const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    C,
    Cpp,
    Java,
    Kotlin,
    Swift,
    Bash,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
    Sql,
    Diff,
    Plain,
}

/// Text that continues onto the next line of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Comment,
    String(&'static str),
}

/// Highlights the lines of one code block in order.
pub(super) struct Highlighter {
    language: Language,
    open: Option<Open>,
}

/// Highlights a single line with no surrounding block, for diff views.
pub fn render_line(language: &str, line: &str) -> String {
    Highlighter::new(language).line(line)
}

impl Highlighter {
    pub(super) fn new(language: &str) -> Self {
        Self {
            language: Language::from_name(language),
            open: None,
        }
    }

    pub(super) fn line(&mut self, line: &str) -> String {
        let line = sanitize(line);
        match self.language {
            Language::Plain => return line,
            Language::Diff => return diff_line(&line),
            _ => {}
        }
        let palette = theme::palette();
        let chars: Vec<char> = line.chars().collect();
        let mut output = String::with_capacity(line.len() + 32);
        let mut index = 0usize;

        if let Some(open) = self.open {
            let (style, closer) = match open {
                Open::Comment => (
                    palette.comment,
                    self.language.block_comment().map_or("", |(_, close)| close),
                ),
                Open::String(delimiter) => (palette.string, delimiter),
            };
            match find(&chars, 0, closer) {
                Some(close) => {
                    index = close + closer.chars().count();
                    styled(&mut output, style, &chars[..index]);
                    self.open = None;
                }
                None => {
                    styled(&mut output, style, &chars);
                    return output;
                }
            }
        }

        if self.language == Language::Toml {
            let start = chars.iter().take_while(|c| c.is_whitespace()).count();
            if start >= index && chars.get(start) == Some(&'[') {
                output.extend(&chars[index..start]);
                styled(&mut output, palette.type_name, &chars[start..]);
                return output;
            }
        }

        // Shells name a command first on each line and after separators.
        let mut command = self.language == Language::Bash;
        while index < chars.len() {
            let character = chars[index];
            if character.is_whitespace() {
                output.push(character);
                index += 1;
                continue;
            }
            if self.line_comment_at(&chars, index) {
                styled(&mut output, palette.comment, &chars[index..]);
                break;
            }
            if let Some((open, close)) = self.language.block_comment()
                && starts_with(&chars, index, open)
            {
                match find(&chars, index + open.chars().count(), close) {
                    Some(found) => {
                        let end = found + close.chars().count();
                        styled(&mut output, palette.comment, &chars[index..end]);
                        index = end;
                    }
                    None => {
                        styled(&mut output, palette.comment, &chars[index..]);
                        self.open = Some(Open::Comment);
                        break;
                    }
                }
                continue;
            }
            if let Some(delimiter) = self.string_delimiter(&chars, index) {
                let Some(end) = string_end(&chars, index + delimiter.chars().count(), delimiter)
                else {
                    styled(&mut output, palette.string, &chars[index..]);
                    if is_multi_line(delimiter) {
                        self.open = Some(Open::String(delimiter));
                    }
                    break;
                };
                // A quoted name before `:` or `=` is a key.
                let style = match self.language.key_delimiter() {
                    Some(key) if next_non_space(&chars, end) == Some(key) => palette.function,
                    _ => palette.string,
                };
                styled(&mut output, style, &chars[index..end]);
                index = end;
                continue;
            }
            if self.language == Language::Bash && character == '$' {
                index = self.shell_dollar(&chars, index, &mut output, &mut command);
                continue;
            }
            if self.language == Language::Bash
                && character == '-'
                && (index == 0 || chars[index - 1].is_whitespace())
                && chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '-')
            {
                let end = index
                    + chars[index..]
                        .iter()
                        .take_while(|c| !c.is_whitespace() && **c != '=')
                        .count();
                styled(&mut output, palette.variable, &chars[index..end]);
                index = end;
                command = false;
                continue;
            }
            if character.is_ascii_digit() {
                let end = index
                    + chars[index..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
                        .count();
                styled(&mut output, palette.number, &chars[index..end]);
                index = end;
                continue;
            }
            if let Some((end, style)) = self.sigil_token(&chars, index) {
                styled(&mut output, style, &chars[index..end]);
                index = end;
                continue;
            }
            if self.language.is_name_start(character) {
                let end = index
                    + chars[index..]
                        .iter()
                        .take_while(|c| self.language.is_name_continue(**c))
                        .count();
                let token: String = chars[index..end].iter().collect();
                match self.classify(&token, &chars, index, end, command) {
                    Some(style) => styled(&mut output, style, &chars[index..end]),
                    None => output.push_str(&token),
                }
                if self.language == Language::Bash {
                    command = shell_expects_command(&token, &chars, index, end, command);
                }
                index = end;
                continue;
            }
            output.push(character);
            index += 1;
            if self.language == Language::Bash
                && matches!(character, '|' | ';' | '&' | '(' | '`' | '{' | '!')
            {
                command = true;
            }
        }
        output
    }

    /// Whether a line comment starts at `index`. `#` counts only at the
    /// start of the line or after a space, so `a#b` and `url#frag` stay
    /// plain in shells and YAML.
    fn line_comment_at(&self, chars: &[char], index: usize) -> bool {
        let Some(marker) = self.language.line_comment() else {
            return false;
        };
        starts_with(chars, index, marker)
            && (marker != "#" || index == 0 || chars[index - 1].is_whitespace())
    }

    fn string_delimiter(&self, chars: &[char], index: usize) -> Option<&'static str> {
        match chars[index] {
            '"' if self.language.triple_quotes() && starts_with(chars, index, "\"\"\"") => {
                Some("\"\"\"")
            }
            '"' => Some("\""),
            '\'' if self.language == Language::Json => None,
            // Rust: `'a'` and `'\n'` are characters, `'a` is a lifetime.
            '\'' if self.language == Language::Rust => (chars.get(index + 1) == Some(&'\\')
                || chars.get(index + 2) == Some(&'\''))
            .then_some("'"),
            '\'' if self.language == Language::Python && starts_with(chars, index, "'''") => {
                Some("'''")
            }
            '\'' => Some("'"),
            '`' if matches!(
                self.language,
                Language::JavaScript | Language::TypeScript | Language::Go
            ) =>
            {
                Some("`")
            }
            _ => None,
        }
    }

    /// `$VAR`, `${...}`, `$1`, `$?` and friends, `$(` opening a command,
    /// and a bare `$ ` prompt at the start of a console line.
    fn shell_dollar(
        &self,
        chars: &[char],
        index: usize,
        output: &mut String,
        command: &mut bool,
    ) -> usize {
        let palette = theme::palette();
        match chars.get(index + 1).copied() {
            Some('(') => {
                output.push_str("$(");
                *command = true;
                index + 2
            }
            Some('{') => {
                let end = chars[index..]
                    .iter()
                    .position(|c| *c == '}')
                    .map_or(chars.len(), |close| index + close + 1);
                styled(output, palette.variable, &chars[index..end]);
                *command = false;
                end
            }
            Some(next) if next.is_ascii_alphanumeric() || next == '_' => {
                let end = index
                    + 1
                    + chars[index + 1..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                        .count();
                styled(output, palette.variable, &chars[index..end]);
                *command = false;
                end
            }
            Some('?' | '@' | '#' | '*' | '!' | '$' | '-') => {
                styled(output, palette.variable, &chars[index..index + 2]);
                *command = false;
                index + 2
            }
            next if next.is_none_or(char::is_whitespace)
                && chars[..index].iter().all(|c| c.is_whitespace()) =>
            {
                styled(output, palette.muted, &['$']);
                index + 1
            }
            _ => {
                output.push('$');
                index + 1
            }
        }
    }

    /// Tokens introduced by a symbol: `@decorator`, `#include`, and CSS
    /// `.class`, `#id`, and `#hex` values.
    fn sigil_token(&self, chars: &[char], index: usize) -> Option<(usize, &'static str)> {
        let palette = theme::palette();
        let sigil = chars[index];
        let next = *chars.get(index + 1)?;
        if !self.language.is_name_start(next) && !(sigil == '#' && next.is_ascii_hexdigit()) {
            return None;
        }
        let end = index
            + 1
            + chars[index + 1..]
                .iter()
                .take_while(|c| self.language.is_name_continue(**c))
                .count();
        let style = match (sigil, self.language) {
            (
                '@',
                Language::Python
                | Language::Java
                | Language::Kotlin
                | Language::Swift
                | Language::TypeScript
                | Language::JavaScript
                | Language::Css,
            ) => palette.function,
            ('#', Language::C | Language::Cpp) => palette.keyword,
            ('#', Language::Css) => {
                let digits = end - index - 1;
                if matches!(digits, 3 | 4 | 6 | 8)
                    && chars[index + 1..end].iter().all(char::is_ascii_hexdigit)
                {
                    palette.number
                } else {
                    palette.type_name
                }
            }
            ('.', Language::Css) => palette.type_name,
            _ => return None,
        };
        Some((end, style))
    }

    fn classify(
        &self,
        token: &str,
        chars: &[char],
        start: usize,
        end: usize,
        command: bool,
    ) -> Option<&'static str> {
        let palette = theme::palette();
        let language = self.language;
        if language.is_keyword(token) {
            return Some(palette.keyword);
        }
        if is_literal(token) {
            return Some(palette.literal);
        }
        let next = chars.get(end).copied();
        match language {
            Language::Bash => {
                // The value of `NAME=value` is neither a command nor a name.
                if start > 0 && chars[start - 1] == '=' {
                    None
                } else if command && next == Some('=') {
                    Some(palette.variable)
                } else if command {
                    Some(palette.function)
                } else {
                    None
                }
            }
            Language::Html => {
                // The two non-space characters before the name, nearest first.
                let before: String = chars[..start]
                    .iter()
                    .rev()
                    .skip_while(|c| c.is_whitespace())
                    .take(2)
                    .collect();
                if before.starts_with('<') || (before.starts_with('/') && before.ends_with('<')) {
                    Some(palette.keyword)
                } else if next == Some('=') {
                    Some(palette.type_name)
                } else {
                    None
                }
            }
            _ => {
                if let Some(key) = language.key_delimiter()
                    && next_non_space(chars, end) == Some(key)
                {
                    let after = chars
                        .iter()
                        .skip(end)
                        .skip_while(|c| c.is_whitespace())
                        .nth(1);
                    if after.is_none_or(|c| c.is_whitespace()) {
                        return Some(palette.function);
                    }
                }
                if language.has_types()
                    && token.starts_with(|c: char| c.is_ascii_uppercase())
                    && token.chars().any(|c| c.is_ascii_lowercase())
                {
                    return Some(palette.type_name);
                }
                let is_call = next == Some('(')
                    || (language == Language::Rust
                        && next == Some('!')
                        && matches!(chars.get(end + 1), Some('(' | '[' | '{')));
                if is_call && language.has_calls() {
                    return Some(palette.function);
                }
                None
            }
        }
    }
}

impl Language {
    fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "rs" | "rust" => Self::Rust,
            "py" | "python" | "python3" => Self::Python,
            "js" | "javascript" | "jsx" | "mjs" | "cjs" | "node" => Self::JavaScript,
            "ts" | "typescript" | "tsx" => Self::TypeScript,
            "go" | "golang" => Self::Go,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" => Self::Cpp,
            "java" => Self::Java,
            "kt" | "kts" | "kotlin" => Self::Kotlin,
            "swift" => Self::Swift,
            "sh" | "shell" | "bash" | "zsh" | "fish" | "console" | "shell-session"
            | "shellsession" | "terminal" => Self::Bash,
            "json" | "jsonc" | "json5" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => Self::Html,
            "css" | "scss" | "less" => Self::Css,
            "sql" | "psql" | "postgresql" | "mysql" | "sqlite" => Self::Sql,
            "diff" | "patch" => Self::Diff,
            _ => Self::Plain,
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Rust
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::C
            | Self::Cpp
            | Self::Java
            | Self::Kotlin
            | Self::Swift => Some("//"),
            Self::Python | Self::Bash | Self::Toml | Self::Yaml => Some("#"),
            Self::Sql => Some("--"),
            Self::Json | Self::Html | Self::Css | Self::Diff | Self::Plain => None,
        }
    }

    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Rust
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::C
            | Self::Cpp
            | Self::Java
            | Self::Kotlin
            | Self::Swift
            | Self::Css
            | Self::Sql => Some(("/*", "*/")),
            Self::Html => Some(("<!--", "-->")),
            _ => None,
        }
    }

    fn triple_quotes(self) -> bool {
        matches!(self, Self::Python | Self::Kotlin | Self::Swift)
    }

    /// The character that follows a key: JSON, YAML, and CSS use `:`,
    /// TOML uses `=`.
    fn key_delimiter(self) -> Option<char> {
        match self {
            Self::Json | Self::Yaml | Self::Css => Some(':'),
            Self::Toml => Some('='),
            _ => None,
        }
    }

    /// Languages where a capitalized name is a type.
    fn has_types(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::Python
                | Self::JavaScript
                | Self::TypeScript
                | Self::Go
                | Self::C
                | Self::Cpp
                | Self::Java
                | Self::Kotlin
                | Self::Swift
        )
    }

    /// Languages where `name(` is a call.
    fn has_calls(self) -> bool {
        self.has_types() || self == Self::Sql
    }

    fn is_name_start(self, character: char) -> bool {
        character.is_ascii_alphabetic()
            || character == '_'
            || (character == '$' && matches!(self, Self::JavaScript | Self::TypeScript))
            || (matches!(character, '/' | '.' | '~') && self == Self::Bash)
    }

    fn is_name_continue(self, character: char) -> bool {
        if character.is_ascii_alphanumeric() || character == '_' {
            return true;
        }
        match self {
            Self::Bash => matches!(character, '-' | '.' | '/' | '~' | '+' | ':' | '@' | '%'),
            Self::Css | Self::Html => character == '-',
            Self::Yaml | Self::Toml => matches!(character, '-' | '.'),
            Self::JavaScript | Self::TypeScript => character == '$',
            _ => false,
        }
    }

    fn is_keyword(self, token: &str) -> bool {
        if self == Self::Sql {
            let upper = token.to_ascii_uppercase();
            return SQL_KEYWORDS.contains(&upper.as_str());
        }
        let keywords: &[&str] = match self {
            Self::Rust => RUST_KEYWORDS,
            Self::Python => PYTHON_KEYWORDS,
            Self::JavaScript | Self::TypeScript => JAVASCRIPT_KEYWORDS,
            Self::Go => GO_KEYWORDS,
            Self::C | Self::Cpp => C_KEYWORDS,
            Self::Java => JAVA_KEYWORDS,
            Self::Kotlin => KOTLIN_KEYWORDS,
            Self::Swift => SWIFT_KEYWORDS,
            Self::Bash => BASH_KEYWORDS,
            _ => &[],
        };
        keywords.contains(&token)
    }
}

/// Whether the next shell word names a command, after the word at
/// `start..end` was read with `command` telling whether that word was one.
fn shell_expects_command(
    token: &str,
    chars: &[char],
    start: usize,
    end: usize,
    command: bool,
) -> bool {
    const WRAPPERS: &[&str] = &[
        "sudo", "doas", "exec", "command", "builtin", "time", "nohup", "env", "xargs", "nice",
        "watch",
    ];
    const OPENERS: &[&str] = &["if", "then", "elif", "else", "while", "until", "do"];
    if BASH_KEYWORDS.contains(&token) {
        return OPENERS.contains(&token);
    }
    // `NAME=value command`: the name and its value leave the position open.
    if start > 0 && chars[start - 1] == '=' {
        return command;
    }
    if command && chars.get(end) == Some(&'=') {
        return true;
    }
    command && WRAPPERS.contains(&token)
}

/// Whole-line diff coloring: added and removed lines, hunk headers, and
/// file headers.
fn diff_line(line: &str) -> String {
    let palette = theme::palette();
    let style = if line.starts_with("+++") || line.starts_with("---") {
        palette.muted
    } else if line.starts_with('+') {
        palette.tool_added
    } else if line.starts_with('-') {
        palette.tool_removed
    } else if line.starts_with("@@") {
        palette.literal
    } else if [
        "diff ",
        "index ",
        "new file",
        "deleted file",
        "similarity",
        "rename ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    {
        palette.muted
    } else {
        return line.to_string();
    };
    format!("{style}{line}{RESET}")
}

fn is_multi_line(delimiter: &str) -> bool {
    matches!(delimiter, "`" | "\"\"\"" | "'''")
}

/// Index just past the closing `delimiter`, honoring backslash escapes.
fn string_end(chars: &[char], mut index: usize, delimiter: &str) -> Option<usize> {
    let length = delimiter.chars().count();
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if starts_with(chars, index, delimiter) {
            return Some(index + length);
        }
        index += 1;
    }
    None
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|index| starts_with(chars, *index, needle))
}

fn next_non_space(chars: &[char], index: usize) -> Option<char> {
    chars[index.min(chars.len())..]
        .iter()
        .copied()
        .find(|c| !c.is_whitespace())
}

fn starts_with(chars: &[char], index: usize, needle: &str) -> bool {
    let Some(suffix) = chars.get(index..) else {
        return false;
    };
    let mut suffix = suffix.iter().copied();
    needle
        .chars()
        .all(|expected| suffix.next() == Some(expected))
}

fn is_literal(token: &str) -> bool {
    matches!(
        token,
        "true" | "false" | "null" | "None" | "True" | "False" | "nil" | "undefined" | "NULL"
    )
}

fn styled(output: &mut String, style: &str, text: &[char]) {
    output.push_str(style);
    output.extend(text);
    output.push_str(RESET);
}

fn sanitize(text: &str) -> String {
    crate::text::sanitize_line(text, crate::text::Tabs::Space)
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "match", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

const JAVASCRIPT_KEYWORDS: &[&str] = &[
    "abstract",
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
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "namespace",
    "new",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "satisfies",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

const C_KEYWORDS: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "explicit",
    "extern",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "try",
    "var",
    "void",
    "volatile",
    "while",
];

const KOTLIN_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "break",
    "by",
    "class",
    "companion",
    "const",
    "continue",
    "data",
    "do",
    "else",
    "enum",
    "for",
    "fun",
    "if",
    "import",
    "in",
    "interface",
    "internal",
    "is",
    "lateinit",
    "object",
    "open",
    "override",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "sealed",
    "suspend",
    "this",
    "throw",
    "try",
    "typealias",
    "val",
    "var",
    "when",
    "while",
];

const SWIFT_KEYWORDS: &[&str] = &[
    "actor",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "continue",
    "default",
    "defer",
    "do",
    "else",
    "enum",
    "extension",
    "fallthrough",
    "fileprivate",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "inout",
    "internal",
    "is",
    "let",
    "mutating",
    "open",
    "override",
    "private",
    "protocol",
    "public",
    "repeat",
    "return",
    "self",
    "Self",
    "some",
    "static",
    "struct",
    "subscript",
    "switch",
    "throw",
    "throws",
    "try",
    "typealias",
    "var",
    "where",
    "while",
];

const BASH_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in", "select",
    "then", "until", "while",
];

const SQL_KEYWORDS: &[&str] = &[
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "COMMIT",
    "CREATE",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "FROM",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "UNION",
    "UPDATE",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> &'static theme::Palette {
        theme::palette()
    }

    fn span(style: &str, text: &str) -> String {
        format!("{style}{text}{RESET}")
    }

    #[test]
    fn rust_keywords_strings_and_comments_are_styled() {
        let rendered = render_line("rust", "let name = \"yawl\"; // note");
        assert!(rendered.contains(&span(palette().keyword, "let")));
        assert!(rendered.contains(&span(palette().string, "\"yawl\"")));
        assert!(rendered.contains(&span(palette().comment, "// note")));
    }

    #[test]
    fn unknown_languages_are_plain_and_sanitized() {
        assert_eq!(render_line("wat", "a\x1bb"), "a�b");
        assert_eq!(render_line("", "let x"), "let x");
        assert_eq!(render_line("text", "let x"), "let x");
    }

    #[test]
    fn rust_types_calls_macros_and_lifetimes() {
        let rendered = render_line("rust", "fn run(name: &'a str) -> Vec<u8> { println!('x') }");
        assert!(rendered.contains(&span(palette().function, "run")));
        // Lifetimes are not strings, so the rest of the line keeps its colors.
        assert!(rendered.contains("&'a str"), "{rendered:?}");
        assert!(rendered.contains(&span(palette().type_name, "Vec")));
        assert!(rendered.contains(&span(palette().function, "println")));
        assert!(rendered.contains(&span(palette().string, "'x'")));
    }

    #[test]
    fn shell_commands_flags_variables_and_prompts() {
        let rendered = render_line(
            "bash",
            "$ sudo rm -rf \"$HOME/tmp\" ~/cache --force=yes | grep ${PATTERN} && FOO=1 ./run.sh # done",
        );
        assert!(
            rendered.starts_with(&span(palette().muted, "$")),
            "{rendered:?}"
        );
        assert!(rendered.contains(&span(palette().function, "sudo")));
        assert!(rendered.contains(&span(palette().function, "rm")));
        assert!(rendered.contains(&span(palette().variable, "-rf")));
        assert!(rendered.contains(&span(palette().string, "\"$HOME/tmp\"")));
        assert!(
            rendered.contains(" ~/cache "),
            "paths stay plain: {rendered:?}"
        );
        assert!(rendered.contains(&span(palette().variable, "--force")));
        assert!(rendered.contains(&span(palette().function, "grep")));
        assert!(rendered.contains(&span(palette().variable, "${PATTERN}")));
        assert!(rendered.contains(&span(palette().variable, "FOO")));
        assert!(rendered.contains(&span(palette().function, "./run.sh")));
        assert!(rendered.contains(&span(palette().comment, "# done")));
        let plain = render_line("sh", "echo a#b");
        assert!(!plain.contains(palette().comment));
    }

    #[test]
    fn shell_keywords_open_a_command_position() {
        let rendered = render_line("bash", "if test -f x; then cat x; fi");
        assert!(rendered.contains(&span(palette().keyword, "if")));
        assert!(rendered.contains(&span(palette().function, "test")));
        assert!(rendered.contains(&span(palette().function, "cat")));
        assert!(rendered.contains(&span(palette().keyword, "fi")));
        assert!(!rendered.contains(&span(palette().function, "x")));
    }

    #[test]
    fn json_and_yaml_and_toml_keys_differ_from_values() {
        let json = render_line("json", "{\"name\": \"yawl\", \"size\": 3, \"ok\": true}");
        assert!(json.contains(&span(palette().function, "\"name\"")));
        assert!(json.contains(&span(palette().string, "\"yawl\"")));
        assert!(json.contains(&span(palette().number, "3")));
        assert!(json.contains(&span(palette().literal, "true")));

        let yaml = render_line("yaml", "image: nginx:1.25 # web");
        assert!(yaml.contains(&span(palette().function, "image")));
        assert!(!yaml.contains(&span(palette().function, "nginx")));
        assert!(yaml.contains(&span(palette().comment, "# web")));
        let url = render_line("yaml", "url: http://example.com");
        assert!(!url.contains(&span(palette().function, "http")));

        let toml = render_line("toml", "edition = \"2024\"");
        assert!(toml.contains(&span(palette().function, "edition")));
        let table = render_line("toml", "[dependencies]");
        assert_eq!(table, span(palette().type_name, "[dependencies]"));
    }

    #[test]
    fn block_comments_and_strings_continue_across_lines() {
        let mut highlighter = Highlighter::new("ts");
        let first = highlighter.line("const a = 1; /* start");
        assert!(first.contains(&span(palette().keyword, "const")));
        assert!(first.ends_with(&span(palette().comment, "/* start")));
        assert_eq!(
            highlighter.line("still comment"),
            span(palette().comment, "still comment")
        );
        let last = highlighter.line("end */ let b = `x");
        assert!(last.starts_with(&span(palette().comment, "end */")));
        assert!(last.ends_with(&span(palette().string, "`x")));
        assert_eq!(
            highlighter.line("y` + f()"),
            format!(
                "{} + {}()",
                span(palette().string, "y`"),
                span(palette().function, "f")
            )
        );

        let mut python = Highlighter::new("python");
        python.line("def f():");
        python.line("    \"\"\"Docs");
        assert_eq!(
            python.line("    more\"\"\""),
            span(palette().string, "    more\"\"\"")
        );
        assert!(
            python
                .line("return 1")
                .contains(&span(palette().keyword, "return"))
        );
    }

    #[test]
    fn diff_lines_color_by_prefix() {
        assert_eq!(
            render_line("diff", "+added"),
            span(palette().tool_added, "+added")
        );
        assert_eq!(
            render_line("diff", "-removed"),
            span(palette().tool_removed, "-removed")
        );
        assert_eq!(
            render_line("diff", "@@ -1,2 +1,2 @@"),
            span(palette().literal, "@@ -1,2 +1,2 @@")
        );
        assert_eq!(
            render_line("diff", "--- a/file"),
            span(palette().muted, "--- a/file")
        );
        assert_eq!(render_line("patch", " context"), " context");
    }

    #[test]
    fn sql_html_and_css_tokens() {
        let sql = render_line("sql", "select count(*) from users where id = 1; -- all");
        assert!(sql.contains(&span(palette().keyword, "select")));
        assert!(sql.contains(&span(palette().function, "count")));
        assert!(sql.contains(&span(palette().comment, "-- all")));

        let html = render_line("html", "<div class=\"box\"><!-- note -->text</div>");
        assert!(html.contains(&span(palette().keyword, "div")));
        assert!(html.contains(&span(palette().type_name, "class")));
        assert!(html.contains(&span(palette().comment, "<!-- note -->")));
        assert!(html.contains(&format!("{RESET}text<")), "{html:?}");

        let css = render_line("css", ".card { color: #fff; margin: 0 }");
        assert!(css.contains(&span(palette().type_name, ".card")));
        assert!(css.contains(&span(palette().function, "color")));
        assert!(css.contains(&span(palette().number, "#fff")));
        assert!(css.contains(&span(palette().function, "margin")));
    }

    #[test]
    fn decorators_and_preprocessor_lines() {
        let python = render_line("py", "@dataclass");
        assert_eq!(python, span(palette().function, "@dataclass"));
        let c = render_line("c", "#include <stdio.h>");
        assert!(c.starts_with(&span(palette().keyword, "#include")));
    }
}
