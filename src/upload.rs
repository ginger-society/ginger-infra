use std::path::Path;

use GingerBucket::{
    apis::default_api::{
        upload_routes_create_upload, upload_routes_start_upload,
        UploadRoutesCreateUploadParams, UploadRoutesStartUploadParams,
    },
    get_configuration,
    models::StartUploadRequest,
};
use GingerBucket::apis::Error as ApiError;

use ginger_shared_rs::utils::get_token_from_file_storage;
use tokio::io::AsyncReadExt;

const CHUNK_SIZE: usize = 5 * 1024 * 1024; // 5MB
use indicatif::{ProgressBar, ProgressStyle};

fn multipart_boundary() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("----gingerboundary{:x}", nanos)
}

fn build_multipart_body(boundary: &str, part_number: i32, chunk: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(chunk.len() + 256);

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"part_number\"\r\n\r\n");
    body.extend_from_slice(part_number.to_string().as_bytes());
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"chunk\"; filename=\"chunk\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(chunk);
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    body
}

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
        Err(ApiError::ResponseError(ref e)) if e.status.as_u16() == 409 => {
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

        let chunk = &buf[..bytes_read];
        let boundary = multipart_boundary();
        let body = build_multipart_body(&boundary, part_number, chunk);

        let url = format!("{}/upload/{}", config.base_path, upload_id);

        let mut req = config
            .client
            .post(&url)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(body);

        if let Some(ref ua) = config.user_agent {
            req = req.header("user-agent", ua.clone());
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