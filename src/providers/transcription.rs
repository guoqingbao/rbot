use std::path::Path;

use anyhow::Result;
use reqwest::multipart;

pub struct GroqTranscriptionProvider {
    api_key: String,
    client: reqwest::Client,
}

impl GroqTranscriptionProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    pub async fn transcribe(&self, file_path: &Path) -> Result<String> {
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "audio.ogg".to_string());
        let file_bytes = tokio::fs::read(file_path).await?;
        let mime = mime_guess::from_path(file_path)
            .first_or_octet_stream()
            .to_string();

        let part = multipart::Part::bytes(file_bytes)
            .file_name(file_name)
            .mime_str(&mime)?;
        let form = multipart::Form::new()
            .text("model", "whisper-large-v3-turbo")
            .part("file", part);

        let resp = self
            .client
            .post("https://api.groq.com/openai/v1/audio/transcriptions")
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        Ok(body
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}
