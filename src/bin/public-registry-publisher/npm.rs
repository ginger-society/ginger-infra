use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::{Digest as Sha512Digest, Sha512};
use std::io::Read;
use std::path::Path;

/// npm registry package names must have `/` percent-encoded in URLs (scoped
/// packages), but `@` is left literal. This matches what the npm CLI itself
/// sends.
pub fn encode_pkg_name(pkg: &str) -> String {
    if let Some((scope, name)) = pkg.split_once('/') {
        format!("{}%2f{}", scope, urlencoding::encode(name))
    } else {
        urlencoding::encode(pkg).to_string()
    }
}

pub struct NpmTarball {
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
    pub package_json: Value,
}

/// Extract `package/package.json` from a `.tgz` tarball, same as `tar -xOzf
/// pkg.tgz package/package.json` in the original bash entrypoint.
pub fn read_tarball(path: &Path) -> Result<NpmTarball> {
    let bytes = std::fs::read(path)?;
    let gz = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(gz);

    let mut package_json = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_path_buf();
        if entry_path == Path::new("package/package.json") {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;
            package_json = Some(serde_json::from_str(&content).with_context(|| {
                format!("package/package.json in {} is not valid JSON", path.display())
            })?);
            break;
        }
    }

    Ok(NpmTarball {
        path: path.to_path_buf(),
        bytes,
        package_json: package_json
            .with_context(|| format!("no package/package.json found in {}", path.display()))?,
    })
}

/// GET {registry}/{pkg}/{version}. Returns true if it already exists.
pub async fn version_exists(
    client: &Client,
    registry: &str,
    token: Option<&str>,
    pkg: &str,
    version: &str,
) -> Result<bool> {
    let url = format!(
        "{}/{}/{}",
        registry.trim_end_matches('/'),
        encode_pkg_name(pkg),
        urlencoding::encode(version)
    );
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("checking existing npm version")?;
    match resp.status().as_u16() {
        200 => Ok(true),
        404 => Ok(false),
        other => bail!(
            "unexpected status {} checking {}@{} on {}",
            other,
            pkg,
            version,
            registry
        ),
    }
}

/// PUT {registry}/{pkg} with the attachment-document body, equivalent to
/// `npm publish <tarball> --access public`.
pub async fn publish(
    client: &Client,
    registry: &str,
    token: &str,
    pkg: &str,
    version: &str,
    tarball: &NpmTarball,
) -> Result<()> {
    let filename = tarball
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .context("tarball has no filename")?
        .to_string();

    let mut sha1 = Sha1::new();
    sha1.update(&tarball.bytes);
    let shasum = hex::encode(sha1.finalize());

    let mut sha512 = Sha512::new();
    sha512.update(&tarball.bytes);
    let integrity = format!("sha512-{}", STANDARD.encode(sha512.finalize()));

    let encoded_tarball = STANDARD.encode(&tarball.bytes);

    // The registry tarball URL is informational for a first publish of this
    // version (the registry rewrites it), but must be present.
    let tarball_url = format!(
        "{}/{}/-/{}",
        registry.trim_end_matches('/'),
        encode_pkg_name(pkg),
        filename
    );

    let mut version_doc = tarball.package_json.clone();
    if let Value::Object(ref mut map) = version_doc {
        map.insert(
            "dist".to_string(),
            json!({
                "integrity": integrity,
                "shasum": shasum,
                "tarball": tarball_url,
            }),
        );
    }

    let body = json!({
        "_id": pkg,
        "name": pkg,
        "description": tarball.package_json.get("description").cloned().unwrap_or(Value::Null),
        "dist-tags": { "latest": version },
        "versions": { version: version_doc },
        "access": "public",
        "_attachments": {
            filename.clone(): {
                "content_type": "application/octet-stream",
                "data": encoded_tarball,
                "length": tarball.bytes.len(),
            }
        }
    });

    let url = format!("{}/{}", registry.trim_end_matches('/'), encode_pkg_name(pkg));

    let resp = client
        .put(&url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .context("sending publish request to npm registry")?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }

    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 409 || body.to_lowercase().contains("cannot publish over") {
        // Published concurrently by another process - treat as a skip, same
        // as the original bash entrypoint did.
        return Ok(());
    }

    bail!(
        "npm publish of {}@{} failed: HTTP {} - {}",
        pkg,
        version,
        status,
        body.trim()
    );
}