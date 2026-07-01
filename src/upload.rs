use std::path::Path;

use GingerBucket::{
    apis::default_api::{
        upload_routes_create_upload, upload_routes_start_upload, upload_routes_upload_part,
        UploadRoutesCreateUploadParams, UploadRoutesStartUploadParams, UploadRoutesUploadPartParams,
    },
    get_configuration,
    models::StartUploadRequest,
};
use GingerBucket::apis::Error as ApiError;

use ginger_shared_rs::utils::get_token_from_file_storage;
use reqwest::multipart::Form;
use tokio::io::AsyncReadExt;


const CHUNK_SIZE: usize = 5 * 1024 * 1024; // 5MB
use indicatif::{ProgressBar, ProgressStyle};

pub async fn run_upload(bucket_path: &str, file: &str, overwrite: bool) {
    let path = Path::new(file);

    let filename = match path.file_name() {
        Some(f) => f.to_string_lossy().to_string(),
        None => {
            eprintln!("Could not determine filename from path: {}", file);
            std::process::exit(1);
        }
    };

    let api_bucket_path = bucket_path.trim_matches('/').to_string();

    let token = get_token_from_file_storage();
    let config = get_configuration(Some(token));

    // 1. Start upload session
    let start_response = match upload_routes_start_upload(
        &config,
        UploadRoutesStartUploadParams {
            bucket_path: api_bucket_path.clone(),
            filename: filename.clone(),
            start_upload_request: StartUploadRequest { overwrite: Some(overwrite) },
        },
    )
    .await
    {
        Ok(r) => r,
        Err(ApiError::ResponseError(ref e)) if e.status == reqwest::StatusCode::CONFLICT => {
            eprintln!(
                "❌ File already exists at '{}/{}'.",
                api_bucket_path, filename
            );
            eprintln!("   Re-run with --overwrite to replace it.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to start upload: {:?}", e);
            std::process::exit(1);
        }
    };

    let upload_id = start_response.upload_id;

    // 2. Read and send chunks
    let mut file_handle = match tokio::fs::File::open(file).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open file {}: {}", file, e);
            std::process::exit(1);
        }
    };

    let total_size = file_handle
        .metadata()
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    let mut part_number: i32 = 0;
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        let bytes_read = match file_handle.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                pb.finish_and_clear();
                eprintln!("Failed to read file: {}", e);
                std::process::exit(1);
            }
        };

        let chunk = buf[..bytes_read].to_vec();

        let form = Form::new()
            .text("part_number", part_number.to_string())
            .part(
                "chunk",
                reqwest::multipart::Part::bytes(chunk)
                    .file_name("chunk")
                    .mime_str("application/octet-stream")
                    .unwrap(),
            );

        let url = format!("{}/upload/{}", config.base_path, upload_id);

        let mut req = config.client.post(&url).multipart(form);
        if let Some(ref ua) = config.user_agent {
            req = req.header(reqwest::header::USER_AGENT, ua.clone());
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                pb.inc(bytes_read as u64);
            }
            Ok(resp) => {
                pb.finish_and_clear();
                eprintln!("Part {} failed: {}", part_number, resp.status());
                std::process::exit(1);
            }
            Err(e) => {
                pb.finish_and_clear();
                eprintln!("Part {} request failed: {:?}", part_number, e);
                std::process::exit(1);
            }
        }

        part_number += 1;
    }

    pb.finish_with_message("upload finished, finalizing...");

    // 3. Finalize
    match upload_routes_create_upload(
        &config,
        UploadRoutesCreateUploadParams {
            upload_id: upload_id.clone(),
        },
    )
    .await
    {
        Ok(r) => {
            println!("✅ Upload complete: {} ({} bytes)", r.bucket_path, r.total_bytes);
        }
        Err(e) => {
            eprintln!("Failed to finalize upload: {:?}", e);
            std::process::exit(1);
        }
    }
}