//! Media download utilities for Telegram photos and documents.

use anyhow::{bail, Context};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use seidrum_common::events::Attachment;
use tracing::debug;

/// Download a photo from Telegram and return it as a base64-encoded Attachment.
///
/// Photos are small enough to inline as base64 data in the event payload.
pub async fn download_photo(
    token: &str,
    file_id: &str,
    file_size: Option<u64>,
) -> anyhow::Result<Attachment> {
    let file_path = get_telegram_file_path(token, file_id).await?;
    let bytes = download_telegram_file(token, &file_path).await?;

    let mime_type = mime_from_extension(&file_path);
    let base64_data = BASE64.encode(&bytes);

    let size = file_size.unwrap_or(bytes.len() as u64);

    debug!(
        file_id = %file_id,
        mime_type = %mime_type,
        size = size,
        "Downloaded photo"
    );

    Ok(Attachment {
        file_type: "image".to_string(),
        url: None,
        file_id: Some(file_id.to_string()),
        mime_type,
        size_bytes: size,
        data: Some(base64_data),
    })
}

/// Download document metadata from Telegram and return as an Attachment.
///
/// Documents can be large, so we store only the file_id for later retrieval
/// rather than inlining the content.
pub async fn download_document(
    token: &str,
    file_id: &str,
    mime_type: Option<String>,
    file_size: Option<u64>,
) -> anyhow::Result<Attachment> {
    // We still call getFile to validate the file_id and get metadata,
    // but we don't download the actual content for documents.
    let file_path = get_telegram_file_path(token, file_id).await?;

    let resolved_mime = mime_type.unwrap_or_else(|| mime_from_extension(&file_path));
    let size = file_size.unwrap_or(0);

    debug!(
        file_id = %file_id,
        mime_type = %resolved_mime,
        size = size,
        "Resolved document metadata"
    );

    Ok(Attachment {
        file_type: "document".to_string(),
        url: None,
        file_id: Some(file_id.to_string()),
        mime_type: resolved_mime,
        size_bytes: size,
        data: None,
    })
}

/// Call Telegram's getFile API to retrieve the file_path for downloading.
async fn get_telegram_file_path(token: &str, file_id: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://api.telegram.org/bot{}/getFile?file_id={}",
        token, file_id
    );

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to call Telegram getFile API")?;

    let body: serde_json::Value = resp
        .json::<serde_json::Value>()
        .await
        .context("Failed to parse getFile response")?;

    if !body["ok"].as_bool().unwrap_or(false) {
        bail!(
            "Telegram getFile API error: {}",
            body["description"].as_str().unwrap_or("unknown error")
        );
    }

    body["result"]["file_path"]
        .as_str()
        .map(|s: &str| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Missing file_path in getFile response"))
}

/// Download a file from Telegram's file storage by its file_path.
async fn download_telegram_file(token: &str, file_path: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        token, file_path
    );

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to download file from Telegram")?;

    if !resp.status().is_success() {
        bail!(
            "Telegram file download failed with status: {}",
            resp.status()
        );
    }

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read file bytes")?;

    Ok(bytes.to_vec())
}

/// Determine MIME type from a file extension.
fn mime_from_extension(path: &str) -> String {
    let lower = path.to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else if lower.ends_with(".mp4") {
        "video/mp4".to_string()
    } else if lower.ends_with(".ogg") || lower.ends_with(".oga") {
        "audio/ogg".to_string()
    } else if lower.ends_with(".mp3") {
        "audio/mpeg".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mime_from_extension_jpg() {
        assert_eq!(mime_from_extension("photos/test.jpg"), "image/jpeg");
    }

    #[test]
    fn test_mime_from_extension_jpeg() {
        assert_eq!(mime_from_extension("photos/test.jpeg"), "image/jpeg");
    }

    #[test]
    fn test_mime_from_extension_png() {
        assert_eq!(mime_from_extension("photos/test.png"), "image/png");
    }

    #[test]
    fn test_mime_from_extension_webp() {
        assert_eq!(mime_from_extension("photos/test.webp"), "image/webp");
    }

    #[test]
    fn test_mime_from_extension_unknown() {
        assert_eq!(
            mime_from_extension("files/test.xyz"),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_mime_from_extension_case_insensitive() {
        assert_eq!(mime_from_extension("photos/test.JPG"), "image/jpeg");
        assert_eq!(mime_from_extension("photos/test.PNG"), "image/png");
    }
}
