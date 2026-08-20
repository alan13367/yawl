//! Small keyword/token highlighter for fenced code blocks. It deliberately
//! handles one line at a time and falls back to escaped plain text for
//! unknown languages.

const RESET: &str = "\x1b[0m";
const KEYWORD: &str = "\x1b[1;34m";
const STRING: &str = "\x1b[32m";
const NUMBER: &str = "\x1b[35m";
const COMMENT: &str = "\x1b[2;36m";
const LITERAL: &str = "\x1b[36m";

pub fn render_line(language: &str, line: &str) -> String {
    let language = normalize_language(language);
    let line = sanitize(line);
    if language.is_empty() || !is_supported(language) {
        return line;
    }

    let chars: Vec<char> = line.chars().collect();
    let mut output = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if let Some(marker) = line_comment(language)
            && starts_with(&chars, index, marker)
        {
            styled(
                &mut output,
                COMMENT,
                &chars[index..].iter().collect::<String>(),
            );
            break;
        }
        if language == "html" && starts_with(&chars, index, "<!--") {
            styled(
                &mut output,
                COMMENT,
                &chars[index..].iter().collect::<String>(),
            );
            break;
        }
        if matches!(
            language,
            "css" | "c" | "cpp" | "go" | "rust" | "javascript" | "typescript"
        ) && starts_with(&chars, index, "/*")
        {
            styled(
                &mut output,
                COMMENT,
                &chars[index..].iter().collect::<String>(),
            );
            break;
        }

        let character = chars[index];
        if matches!(character, '"' | '\'') || character == '`' && supports_backticks(language) {
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < chars.len() {
                let current = chars[index];
                index += 1;
                if current == character && !escaped {
                    break;
                }
                escaped = current == '\\' && !escaped;
                if current != '\\' {
                    escaped = false;
                }
            }
            styled(
                &mut output,
                STRING,
                &chars[start..index].iter().collect::<String>(),
            );
        } else if character.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric()
                    || matches!(chars[index], '.' | '_' | '+' | '-'))
            {
                index += 1;
            }
            styled(
                &mut output,
                NUMBER,
                &chars[start..index].iter().collect::<String>(),
            );
        } else if is_identifier_start(character) {
            let start = index;
            index += 1;
            while index < chars.len() && is_identifier_continue(chars[index]) {
                index += 1;
            }
            let token: String = chars[start..index].iter().collect();
            if is_keyword(language, &token) {
                styled(&mut output, KEYWORD, &token);
            } else if is_literal(&token) {
                styled(&mut output, LITERAL, &token);
            } else {
                output.push_str(&token);
            }
        } else {
            output.push(character);
            index += 1;
        }
    }
    output
}

fn normalize_language(language: &str) -> &str {
    let language = language.trim();
    if ["rs", "rust"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "rust"
    } else if ["py", "python"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "python"
    } else if ["js", "javascript", "jsx"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "javascript"
    } else if ["ts", "typescript", "tsx"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "typescript"
    } else if ["go", "golang"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "go"
    } else if language.eq_ignore_ascii_case("c") {
        "c"
    } else if ["cc", "cpp", "c++", "h", "hpp"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "cpp"
    } else if ["sh", "shell", "bash", "zsh"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "bash"
    } else if ["json", "jsonc"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "json"
    } else if language.eq_ignore_ascii_case("toml") {
        "toml"
    } else if ["html", "htm"]
        .iter()
        .any(|name| language.eq_ignore_ascii_case(name))
    {
        "html"
    } else if language.eq_ignore_ascii_case("css") {
        "css"
    } else {
        ""
    }
}

fn is_supported(language: &str) -> bool {
    matches!(
        language,
        "rust"
            | "python"
            | "javascript"
            | "typescript"
            | "go"
            | "c"
            | "cpp"
            | "bash"
            | "json"
            | "toml"
            | "html"
            | "css"
    )
}

fn line_comment(language: &str) -> Option<&'static str> {
    match language {
        "rust" | "javascript" | "typescript" | "go" | "c" | "cpp" => Some("//"),
        "python" | "bash" | "toml" => Some("#"),
        _ => None,
    }
}

fn supports_backticks(language: &str) -> bool {
    matches!(language, "javascript" | "typescript" | "bash" | "go")
}

fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || matches!(character, '_' | '$')
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit() || character == '-'
}

fn starts_with(chars: &[char], index: usize, needle: &str) -> bool {
    chars[index..]
        .iter()
        .copied()
        .zip(needle.chars())
        .all(|(a, b)| a == b)
        && chars.len().saturating_sub(index) >= needle.chars().count()
}

fn is_literal(token: &str) -> bool {
    matches!(
        token,
        "true" | "false" | "null" | "None" | "True" | "False" | "nil" | "undefined"
    )
}

fn is_keyword(language: &str, token: &str) -> bool {
    let keywords: &[&str] = match language {
        "rust" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        "python" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in",
            "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
            "with", "yield",
        ],
        "javascript" | "typescript" => &[
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
            "new",
            "of",
            "private",
            "protected",
            "public",
            "return",
            "static",
            "switch",
            "throw",
            "try",
            "type",
            "typeof",
            "var",
            "void",
            "while",
            "with",
            "yield",
        ],
        "go" => &[
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
        ],
        "c" | "cpp" => &[
            "auto",
            "bool",
            "break",
            "case",
            "catch",
            "char",
            "class",
            "const",
            "continue",
            "default",
            "delete",
            "do",
            "double",
            "else",
            "enum",
            "extern",
            "float",
            "for",
            "friend",
            "if",
            "inline",
            "int",
            "long",
            "namespace",
            "new",
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
        ],
        "bash" => &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "select", "then", "until", "while",
        ],
        "html" => &[
            "a", "body", "button", "div", "form", "head", "html", "img", "input", "label", "li",
            "link", "main", "meta", "nav", "ol", "p", "script", "section", "span", "style",
            "table", "td", "th", "title", "tr", "ul",
        ],
        "css" => &[
            "align-items",
            "background",
            "border",
            "color",
            "display",
            "flex",
            "font-family",
            "font-size",
            "gap",
            "grid",
            "height",
            "justify-content",
            "margin",
            "padding",
            "position",
            "width",
        ],
        _ => &[],
    };
    keywords.contains(&token)
}

fn styled(output: &mut String, style: &str, text: &str) {
    output.push_str(style);
    output.push_str(text);
    output.push_str(RESET);
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_strings_and_comments_are_styled() {
        let rendered = render_line("rust", "let name = \"yawl\"; // note");
        assert!(rendered.contains("\x1b[1;34mlet\x1b[0m"));
        assert!(rendered.contains("\x1b[32m\"yawl\"\x1b[0m"));
        assert!(rendered.contains("\x1b[2;36m// note\x1b[0m"));
    }

    #[test]
    fn unknown_languages_are_plain_and_sanitized() {
        assert_eq!(render_line("wat", "a\x1bb"), "a�b");
    }
}
