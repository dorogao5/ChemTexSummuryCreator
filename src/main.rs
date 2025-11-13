use anyhow::{Context, Result};
use reqwest::multipart;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::time::{sleep, Duration};

const BASE_URL: &str = "https://texcompile.ru";
const POLL_INTERVAL_SECS: u64 = 5;
const MAX_POLL_ATTEMPTS: u32 = 120;
const REQUEST_TIMOUT_SECS: u64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompilationStatus {
    Queued,
    Processing,
    Completed,
    Failed,
    Unknown(String),
}

impl CompilationStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "Queued" => Self::Queued,
            "Processing" => Self::Processing,
            "Completed" => Self::Completed,
            "Failed" => Self::Failed,
            other => Self::Unknown(other.to_string()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UploadResponse {
    success: bool,
    data: Option<UploadData>,
    error: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadData {
    #[serde(rename = "taskId")]
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    success: bool,
    data: Option<StatusData>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusData {
    status: String,
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
    duration: Option<u64>,
    #[serde(rename = "queuePosition")]
    queue_position: Option<u32>,
}

impl StatusData {
    fn compilation_status(&self) -> CompilationStatus {
        CompilationStatus::from_str(&self.status)
    }

    fn format_duration(&self) -> String {
        self.duration
            .map(format_milliseconds)
            .unwrap_or_else(|| "неизвестно".to_string())
    }
}

fn format_milliseconds(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{} сек.", seconds)
    } else {
        let minutes = seconds / 60;
        let remaining_seconds = seconds % 60;
        if minutes < 60 {
            format! {"{} мин. {} сек.", minutes, remaining_seconds}
        } else {
            let hours = minutes / 60;
            let remaining_minutes = minutes % 60;
            format!("{} ч. {} м.", hours, remaining_minutes)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <path_to_tex_or_zip_file>", args[0]);
        std::process::exit(1);
    }

    let file_path = &args[1];
    compile_and_download(file_path).await?;
    Ok(())
}

async fn compile_and_download(file_path: &str) -> Result<()> {
    println!("Reading files: {}", file_path);
    let file_contents =
        fs::read(file_path).with_context(|| format!("Failed to read file: {}", file_path))?;

    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .context("Invalid file name")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMOUT_SECS))
        .build()
        .context("Failed to create http client")?;

    println!("Uploading file to {}...", BASE_URL);
    let task_id = upload_file(&client, &file_contents, file_name).await?;
    println!("File uploaded. Task ID: {}", task_id);

    println!("Waiting for compilation to complete...");
    let download_url = poll_status(&client, &task_id).await?;

    println!("Downloading PDF from {}", download_url);
    let pdf_bytes = download_pdf(&client, &download_url).await?;

    let output_path = generate_output_path(file_name)?;
    fs::write(&output_path, pdf_bytes)
        .with_context(|| format!("Failed to write PDF file: {}", output_path.display()))?;

    println!("PDF saved to: {}", output_path.display());
    Ok(())
}

async fn upload_file(
    client: &reqwest::Client,
    file_contents: &[u8],
    file_name: &str,
) -> Result<String> {
    let part = multipart::Part::bytes(file_contents.to_vec())
        .file_name(file_name.to_string())
        .mime_str(mime_type_from_filename(file_name)?)
        .context("Failed to set MIME type")?;

    let form = multipart::Form::new().part("texFile", part);

    let response = client
        .post(format!("{}/api/upload", BASE_URL))
        .multipart(form)
        .send()
        .await
        .context("Failed to submit form")?;

    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .context("Failed to read error response")?;
        anyhow::bail!("Upload failed with status {}: {}", status, text);
    }

    let upload_response: UploadResponse = response
        .json()
        .await
        .context("Failed to parse upload response")?;

    if !upload_response.success {
        let error_msg = upload_response
            .error
            .or(upload_response.message)
            .unwrap_or_else(|| "Unknown error".to_string());
        anyhow::bail!("Upload failed: {}", error_msg);
    }

    let task_id = upload_response
        .data
        .map(|d| d.task_id)
        .context("No task ID in response")?;

    Ok(task_id)
}

async fn poll_status(client: &reqwest::Client, task_id: &str) -> Result<String> {
    let poll_interval = Duration::from_secs(POLL_INTERVAL_SECS);

    for attempt in 1..=MAX_POLL_ATTEMPTS {
        let url = format!("{}/api/status/{}", BASE_URL, task_id);
        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to check status")?;

        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .context("Failed to read error response")?;
            anyhow::bail!("Status check failed with status {}: {}", status, text);
        }

        let status_response: StatusResponse = response
            .json()
            .await
            .context("Failed to parse status response")?;

        if !status_response.success {
            let error_msg = status_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("Status check returned error: {}", error_msg);
        }

        let status_data = status_response.data.context("No status data in response")?;

        match status_data.compilation_status() {
            CompilationStatus::Queued => {
                let queue_info = status_data
                    .queue_position
                    .filter(|&pos| pos > 0)
                    .map(|pos| format!(" (position: {})", pos))
                    .unwrap_or_default();
                let duration_info = status_data.format_duration();
                println!(
                    "Status: Queued{} | Time in queue: {}",
                    queue_info, duration_info
                );
            }
            CompilationStatus::Processing => {
                let duration_info = status_data.format_duration();
                println!("Status: Processing... | Time: {}", duration_info);
            }
            CompilationStatus::Completed => {
                println!(
                    "Status: Completed! | Compilation time: {}",
                    status_data.format_duration()
                );
                let download_url = status_data
                    .download_url
                    .context("No download URL in completed status")?;
                return Ok(download_url);
            }
            CompilationStatus::Failed => {
                let duration_info = status_data.format_duration();
                let error_msg = status_data
                    .error_message
                    .as_deref()
                    .unwrap_or("Unknown error");
                anyhow::bail!("Compilation failed after {}: {}", duration_info, error_msg);
            }
            CompilationStatus::Unknown(status) => {
                println!("Status: {} (unknown)", status)
            }
        }

        if attempt < MAX_POLL_ATTEMPTS {
            sleep(poll_interval).await;
        }
    }

    anyhow::bail!("Compilation timeout after {} attempts", MAX_POLL_ATTEMPTS);
}

async fn download_pdf(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let full_url = normalize_url(url);

    let response = client
        .get(&full_url)
        .send()
        .await
        .context("Failed to download PDF")?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Filed to download PDF: status: {}", status);
    }

    let bytes = response
        .bytes()
        .await
        .context("Failed to read PDF bytes")?
        .to_vec();

    Ok(bytes)
}

fn normalize_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{}{}", BASE_URL, url)
    } else {
        format!("{}/{}", BASE_URL, url)
    }
}

fn generate_output_path(input_file_name: &str) -> Result<PathBuf> {
    let output_name = Path::new(input_file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Invalid file name")?;
    Ok(PathBuf::from(format!("{}.pdf", output_name)))
}

fn mime_type_from_filename(filename: &str) -> Result<&'static str> {
    if filename.ends_with(".tex") {
        Ok("text/x-tex")
    } else if filename.ends_with(".zip") {
        Ok("application/zip")
    } else {
        anyhow::bail!("Unsupported file type. Expected .tex or .zip");
    }
}
