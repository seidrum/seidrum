//! Message splitting for Telegram's 4096-character message limit.

/// Default maximum length for a Telegram message chunk.
pub const DEFAULT_MAX_LEN: usize = 4000;

/// Split a message into chunks that fit within Telegram's message size limit.
///
/// Splitting prefers paragraph breaks (`\n\n`), then line breaks (`\n`),
/// then sentence endings (`. `), then the last space. The split point must
/// be past 50% of the slice to avoid tiny leading chunks.
///
/// If the message contains fenced code blocks (` ``` `), the function tracks
/// fence state and properly closes/reopens fences across chunk boundaries.
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut in_code_fence = false;
    let mut fence_lang = String::new();

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let slice = &remaining[..max_len];
        let min_split = max_len / 2;

        // Find best split point, preferring paragraph > line > sentence > space
        let split_at = find_split_point(slice, min_split);

        let (chunk, rest) = remaining.split_at(split_at);
        let mut chunk_str = chunk.to_string();
        remaining = rest;

        // Track code fence state within this chunk
        let fences_in_chunk = count_fences(&chunk_str);
        let was_in_fence = in_code_fence;

        // If we were in a code fence from the previous chunk, prepend the opening fence
        if was_in_fence {
            let prefix = if fence_lang.is_empty() {
                "```\n".to_string()
            } else {
                format!("```{}\n", fence_lang)
            };
            chunk_str = prefix + &chunk_str;
        }

        // Update fence state: each ``` toggles the state
        for _ in 0..fences_in_chunk {
            if !in_code_fence {
                // Entering a fence - capture the language tag
                fence_lang = extract_fence_lang(&chunk_str, in_code_fence);
                in_code_fence = true;
            } else {
                in_code_fence = false;
                fence_lang.clear();
            }
        }

        // If we end inside a code fence, close it
        if in_code_fence && !remaining.is_empty() {
            chunk_str.push_str("\n```");
        }

        chunks.push(chunk_str);
    }

    chunks
}

/// Find the best split point in a slice, preferring paragraph > line > sentence > space.
/// The split point must be at or past `min_split` to avoid tiny chunks.
fn find_split_point(slice: &str, min_split: usize) -> usize {
    // Try paragraph break (\n\n)
    if let Some(pos) = slice.rfind("\n\n") {
        if pos >= min_split {
            return pos + 2; // Include the double newline in the first chunk
        }
    }

    // Try line break (\n)
    if let Some(pos) = slice.rfind('\n') {
        if pos >= min_split {
            return pos + 1;
        }
    }

    // Try sentence end (". ")
    if let Some(pos) = slice.rfind(". ") {
        if pos >= min_split {
            return pos + 2;
        }
    }

    // Try last space
    if let Some(pos) = slice.rfind(' ') {
        if pos >= min_split {
            return pos + 1;
        }
    }

    // No good split point found; hard-split at max_len
    slice.len()
}

/// Count the number of code fence markers (```) in a string.
fn count_fences(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars().peekable();
    let mut at_line_start = true;

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            at_line_start = true;
            continue;
        }

        if at_line_start && ch == '`' {
            let mut backticks = 1;
            while chars.peek() == Some(&'`') {
                chars.next();
                backticks += 1;
            }
            if backticks >= 3 {
                count += 1;
            }
            at_line_start = false;
        } else {
            at_line_start = false;
        }
    }

    count
}

/// Extract the language tag from the last opening fence in the text.
fn extract_fence_lang(text: &str, _was_in_fence: bool) -> String {
    for line in text.lines().rev() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`').trim();
            return lang.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_under_limit_returns_one_chunk() {
        let text = "Hello, world!";
        let result = split_message(text, 100);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "Hello, world!");
    }

    #[test]
    fn test_splits_on_paragraph() {
        let text = format!("{}\n\n{}", "A".repeat(60), "B".repeat(60));
        let result = split_message(&text, 100);
        assert!(result.len() >= 2);
        assert!(result[0].ends_with("\n\n"));
    }

    #[test]
    fn test_splits_on_line() {
        let text = format!("{}\n{}", "A".repeat(60), "B".repeat(60));
        let result = split_message(&text, 100);
        assert!(result.len() >= 2);
    }

    #[test]
    fn test_code_fence_tracking() {
        let code = format!("```rust\n{}\n```", "x".repeat(200));
        let result = split_message(&code, 100);
        assert!(result.len() >= 2);
        // The second chunk should start with a fence reopening
        if result.len() > 1 {
            assert!(result[1].starts_with("```"));
        }
    }

    #[test]
    fn test_multiple_splits() {
        let text = (0..10)
            .map(|i| format!("Paragraph {}. {}", i, "word ".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let result = split_message(&text, 200);
        assert!(result.len() >= 3);
    }

    #[test]
    fn test_empty_string() {
        let result = split_message("", 100);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "");
    }

    #[test]
    fn test_exact_limit() {
        let text = "A".repeat(100);
        let result = split_message(&text, 100);
        assert_eq!(result.len(), 1);
    }
}
