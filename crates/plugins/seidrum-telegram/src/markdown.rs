//! Markdown conversion utilities for Telegram MarkdownV2 and plain text fallback.

use fancy_regex::Regex;

/// Characters that must be escaped in Telegram MarkdownV2 outside of formatting markup.
const SPECIAL_CHARS: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Escape special characters for Telegram MarkdownV2 format.
///
/// This escapes all MarkdownV2 special characters with a preceding backslash.
/// Content inside formatting markup (bold, italic, code, etc.) should be
/// escaped separately by the caller if needed.
pub fn escape_markdown_v2(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if SPECIAL_CHARS.contains(&ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Strip all markdown formatting, returning plain text.
///
/// Handles: bold (**), italic (* and _), underline (__), strikethrough (~~),
/// inline code (`), code blocks (```), links [text](url) -> text,
/// headings (# ...), and blockquotes (> ...).
pub fn strip_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let total_lines = lines.len();

    while i < total_lines {
        let line = lines[i];

        // Fenced code blocks: strip the fences, keep the content
        if line.trim_start().starts_with("```") {
            i += 1;
            while i < total_lines {
                let inner = lines[i];
                if inner.trim_start().starts_with("```") {
                    i += 1;
                    break;
                }
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str(inner);
                i += 1;
            }
            continue;
        }

        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }

        // Headings: strip leading #s
        let stripped_heading = strip_heading(line);
        // Blockquotes: strip leading >
        let stripped_quote = strip_blockquote(stripped_heading);
        // Inline formatting
        let stripped_inline = strip_inline_formatting(stripped_quote);
        result.push_str(&stripped_inline);
        i += 1;
    }

    result
}

/// Strip heading markers (# ## ### etc.) from a line.
fn strip_heading(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let after_hashes = trimmed.trim_start_matches('#');
        after_hashes.strip_prefix(' ').unwrap_or(after_hashes)
    } else {
        line
    }
}

/// Strip blockquote marker (>) from a line.
fn strip_blockquote(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('>') {
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        line
    }
}

/// Strip inline markdown formatting: bold, italic, strikethrough, inline code, links.
fn strip_inline_formatting(text: &str) -> String {
    let mut s = text.to_string();

    // Links: [text](url) -> text
    if let Ok(re) = Regex::new(r"\[([^\]]+)\]\([^)]+\)") {
        s = re.replace_all(&s, "$1").to_string();
    }

    // Bold: ** or __
    s = s.replace("**", "");
    s = s.replace("__", "");

    // Strikethrough: ~~
    s = s.replace("~~", "");

    // Inline code: `text` -> text (just remove backticks)
    s = s.replace('`', "");

    // Single italic markers (* or _) that are not part of ** or __
    // Since we already removed ** and __, remaining * and _ are italic markers
    if let Ok(re) = Regex::new(r"(?<!\w)\*(?!\s)(.+?)(?<!\s)\*(?!\w)") {
        s = re.replace_all(&s, "$1").to_string();
    }
    if let Ok(re) = Regex::new(r"(?<!\w)_(?!\s)(.+?)(?<!\s)_(?!\w)") {
        s = re.replace_all(&s, "$1").to_string();
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_special_chars() {
        let result = escape_markdown_v2("Hello_World! (test) #1");
        assert_eq!(result, "Hello\\_World\\! \\(test\\) \\#1");
    }

    #[test]
    fn test_escape_all_special_chars() {
        let result = escape_markdown_v2("_*[]()~`>#+-=|{}.!");
        assert_eq!(
            result,
            "\\_\\*\\[\\]\\(\\)\\~\\`\\>\\#\\+\\-\\=\\|\\{\\}\\.\\!"
        );
    }

    #[test]
    fn test_escape_empty_input() {
        assert_eq!(escape_markdown_v2(""), "");
    }

    #[test]
    fn test_escape_no_special_chars() {
        assert_eq!(escape_markdown_v2("Hello World"), "Hello World");
    }

    #[test]
    fn test_strip_bold() {
        assert_eq!(strip_markdown("Hello **world**!"), "Hello world!");
    }

    #[test]
    fn test_strip_italic_star() {
        assert_eq!(strip_markdown("Hello *world*!"), "Hello world!");
    }

    #[test]
    fn test_strip_strikethrough() {
        assert_eq!(strip_markdown("Hello ~~world~~!"), "Hello world!");
    }

    #[test]
    fn test_strip_inline_code() {
        assert_eq!(
            strip_markdown("Use `cargo build` here"),
            "Use cargo build here"
        );
    }

    #[test]
    fn test_strip_code_block() {
        let input = "Before\n```rust\nfn main() {}\n```\nAfter";
        let result = strip_markdown(input);
        assert!(result.contains("fn main() {}"));
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("```"));
    }

    #[test]
    fn test_strip_link() {
        assert_eq!(
            strip_markdown("Visit [Google](https://google.com) now"),
            "Visit Google now"
        );
    }

    #[test]
    fn test_strip_heading() {
        assert_eq!(strip_markdown("## Hello World"), "Hello World");
    }

    #[test]
    fn test_strip_blockquote() {
        assert_eq!(strip_markdown("> This is quoted"), "This is quoted");
    }

    #[test]
    fn test_strip_empty_input() {
        assert_eq!(strip_markdown(""), "");
    }

    #[test]
    fn test_strip_multiple_formatting() {
        let input = "**Bold** and *italic* and `code`";
        let result = strip_markdown(input);
        assert_eq!(result, "Bold and italic and code");
    }
}
