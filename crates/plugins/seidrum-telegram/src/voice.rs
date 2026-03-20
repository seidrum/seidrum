//! Voice message transcription using whisper.cpp.
//!
//! Downloads a Telegram voice message (OGG format), converts to WAV via ffmpeg,
//! and transcribes using whisper.cpp CLI.

use anyhow::{bail, Context};
use tempfile::NamedTempFile;
use tracing::{debug, info};

/// Transcribe a Telegram voice message using whisper.cpp.
///
/// # Arguments
/// * `token` - Telegram Bot API token
/// * `file_id` - Telegram file_id for the voice message
/// * `whisper_cli` - Path to the whisper-cli binary
/// * `whisper_model` - Path to the whisper model file
///
/// # Returns
/// `Ok(Some(transcript))` on success, `Ok(None)` if the transcript is empty,
/// or an error if downloading/conversion/transcription fails.
pub async fn transcribe_voice(
    token: &str,
    file_id: &str,
    whisper_cli: &str,
    whisper_model: &str,
) -> anyhow::Result<Option<String>> {
    // Step 1: Get file path from Telegram API
    let file_path = get_telegram_file_path(token, file_id).await?;
    debug!(file_path = %file_path, "Got Telegram file path");

    // Step 2: Download the OGG file
    let ogg_bytes = download_telegram_file(token, &file_path).await?;
    info!(size = ogg_bytes.len(), "Downloaded voice message");

    // Step 3: Save to a temp file with .ogg extension
    let ogg_temp = NamedTempFile::new()
        .context("Failed to create temp file for OGG")?;
    let ogg_path = ogg_temp.path().to_owned();
    let ogg_path_str = format!("{}.ogg", ogg_path.display());
    tokio::fs::write(&ogg_path_str, &ogg_bytes)
        .await
        .context("Failed to write OGG temp file")?;

    // Step 4: Create WAV temp file path
    let wav_temp = NamedTempFile::new()
        .context("Failed to create temp file for WAV")?;
    let wav_path = wav_temp.path().to_owned();
    let wav_path_str = format!("{}.wav", wav_path.display());

    // Step 5: Convert OGG to WAV using ffmpeg
    let ffmpeg_output = tokio::process::Command::new("ffmpeg")
        .args([
            "-i", &ogg_path_str,
            "-ar", "16000",
            "-ac", "1",
            "-y",
            &wav_path_str,
        ])
        .output()
        .await
        .context("Failed to run ffmpeg")?;

    if !ffmpeg_output.status.success() {
        let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr);
        bail!("ffmpeg conversion failed: {}", stderr);
    }
    debug!("Converted OGG to WAV");

    // Step 6: Transcribe using whisper.cpp
    let whisper_output = tokio::process::Command::new(whisper_cli)
        .args([
            "-m", whisper_model,
            "-f", &wav_path_str,
            "--no-timestamps",
            "-nt",
        ])
        .output()
        .await
        .context("Failed to run whisper-cli")?;

    if !whisper_output.status.success() {
        let stderr = String::from_utf8_lossy(&whisper_output.stderr);
        bail!("whisper-cli transcription failed: {}", stderr);
    }

    // Step 7: Parse transcript
    let transcript = String::from_utf8_lossy(&whisper_output.stdout)
        .trim()
        .to_string();

    // Clean up temp files (the .ogg and .wav with appended extensions)
    let _ = tokio::fs::remove_file(&ogg_path_str).await;
    let _ = tokio::fs::remove_file(&wav_path_str).await;
    // The original NamedTempFile handles cleanup of the base paths on drop

    if transcript.is_empty() {
        info!("Whisper returned empty transcript");
        Ok(None)
    } else {
        info!(len = transcript.len(), "Transcription complete");
        Ok(Some(transcript))
    }
}

/// Call Telegram's getFile API to get the file_path for downloading.
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

/// Download a file from Telegram's file storage.
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
        bail!("Telegram file download failed with status: {}", resp.status());
    }

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read file bytes")?;

    Ok(bytes.to_vec())
}
