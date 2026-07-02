use anyhow::{bail, Context, Result};
use md5::{Digest as Md5Digest, Md5};
use reqwest::multipart;
use reqwest::Client;
use sha2::{Digest as Sha256Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// A small subset of the RFC822-style core metadata (PKG-INFO / METADATA)
/// fields Warehouse actually cares about. Single-valued fields keep the last
/// occurrence; multi-valued fields (Classifier, Requires-Dist) collect all
/// occurrences. The long-form "Description" body (which may appear either as
/// a header or as the payload after the blank line) is captured separately.
#[derive(Debug, Default)]
pub struct CoreMetadata {
    pub single: HashMap<String, String>,
    pub multi: HashMap<String, Vec<String>>,
    pub description: Option<String>,
}

/// Fields that Warehouse treats as repeatable and should be sent as multiple
/// form fields with the same name rather than being collapsed.
const MULTI_VALUED_FIELDS: &[&str] = &[
    "Classifier",
    "Requires-Dist",
    "Provides-Dist",
    "Obsoletes-Dist",
    "Project-URL",
    "Platform",
    "Supported-Platform",
    "Dynamic",
];

fn parse_core_metadata(raw: &str) -> CoreMetadata {
    let mut meta = CoreMetadata::default();
    let mut lines = raw.lines().peekable();
    let mut last_key: Option<String> = None;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            // Blank line: everything after this is the long description body
            // (only present in older single-part PKG-INFO/METADATA files).
            let rest: String = lines.collect::<Vec<_>>().join("\n");
            if !rest.trim().is_empty() {
                meta.description = Some(rest);
            }
            break;
        }
        if let Some(stripped) = line.strip_prefix(' ') {
            // Continuation of the previous header (RFC822 folding), used by
            // multi-line Description headers in newer metadata versions.
            if let Some(k) = &last_key {
                if k == "Description" {
                    let entry = meta.single.entry(k.clone()).or_default();
                    entry.push('\n');
                    entry.push_str(stripped.trim_start_matches('|'));
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim().to_string();

        if MULTI_VALUED_FIELDS.contains(&key.as_str()) {
            meta.multi.entry(key.clone()).or_default().push(value);
        } else {
            meta.single.insert(key.clone(), value);
        }
        last_key = Some(key);
    }

    if let Some(d) = meta.single.get("Description") {
        meta.description = Some(d.clone());
    }

    meta
}

pub enum FileKind {
    Sdist,
    Wheel { python_tag: String },
}

pub struct PyDistFile {
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
    pub kind: FileKind,
    pub metadata: CoreMetadata,
}

/// Read a `.tar.gz` sdist and extract its PKG-INFO.
fn read_sdist(path: &Path) -> Result<PyDistFile> {
    let bytes = std::fs::read(path)?;
    let gz = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(gz);
    let mut pkg_info = None;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_path_buf();
        // Layout is always <name>-<version>/PKG-INFO
        if entry_path
            .file_name()
            .map(|n| n == "PKG-INFO")
            .unwrap_or(false)
            && entry_path.components().count() == 2
        {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;
            pkg_info = Some(content);
            break;
        }
    }

    let raw = pkg_info.context("PKG-INFO not found inside sdist (malformed archive?)")?;
    Ok(PyDistFile {
        path: path.to_path_buf(),
        bytes,
        kind: FileKind::Sdist,
        metadata: parse_core_metadata(&raw),
    })
}

/// Read a `.whl` (zip) and extract `<name>-<version>.dist-info/METADATA`.
/// The python tag is parsed from the wheel filename itself, per the wheel
/// spec: `{name}-{version}(-{build})?-{python}-{abi}-{platform}.whl`.
fn read_wheel(path: &Path) -> Result<PyDistFile> {
    let bytes = std::fs::read(path)?;
    let reader = std::io::Cursor::new(&bytes);
    let mut zip = zip::ZipArchive::new(reader)?;

    let mut metadata_content = None;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if file.name().ends_with(".dist-info/METADATA") {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            metadata_content = Some(content);
            break;
        }
    }
    let raw = metadata_content.context("METADATA not found inside wheel (malformed archive?)")?;

    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("wheel filename is not valid UTF-8")?;
    let parts: Vec<&str> = filename.split('-').collect();
    // name-version[-build]-python-abi-platform : at least 5 dash-separated parts.
    if parts.len() < 5 {
        bail!("wheel filename '{}' does not match the wheel spec", filename);
    }
    let python_tag = parts[parts.len() - 3].to_string();

    Ok(PyDistFile {
        path: path.to_path_buf(),
        bytes,
        kind: FileKind::Wheel { python_tag },
        metadata: parse_core_metadata(&raw),
    })
}

pub fn read_dist_file(path: &Path) -> Result<PyDistFile> {
    let name = path.to_string_lossy();
    if name.ends_with(".tar.gz") {
        read_sdist(path)
    } else if name.ends_with(".whl") {
        read_wheel(path)
    } else {
        bail!("unsupported distribution file: {}", name);
    }
}

fn digest_hex(bytes: &[u8]) -> (String, String) {
    let mut md5 = Md5::new();
    md5.update(bytes);
    let md5_hex = hex::encode(md5.finalize());

    let mut sha256 = Sha256::new();
    sha256.update(bytes);
    let sha256_hex = hex::encode(sha256.finalize());

    (md5_hex, sha256_hex)
}

/// Upload a single distribution file to a Warehouse-compatible legacy upload
/// endpoint (`POST {repository_url}`), building the exact multipart form
/// twine sends. Returns Ok(true) if uploaded, Ok(false) if the server
/// reported the file already exists (mirrors `twine --skip-existing`).
pub async fn upload(
    client: &Client,
    repository_url: &str,
    username: &str,
    password: &str,
    dist: &PyDistFile,
) -> Result<bool> {
    let name = dist
        .metadata
        .single
        .get("Name")
        .context("distribution metadata is missing Name")?
        .clone();
    let version = dist
        .metadata
        .single
        .get("Version")
        .context("distribution metadata is missing Version")?
        .clone();
    let metadata_version = dist
        .metadata
        .single
        .get("Metadata-Version")
        .cloned()
        .unwrap_or_else(|| "2.1".to_string());

    let (filetype, pyversion) = match &dist.kind {
        FileKind::Sdist => ("sdist".to_string(), "source".to_string()),
        FileKind::Wheel { python_tag } => ("bdist_wheel".to_string(), python_tag.clone()),
    };

    let (md5_hex, sha256_hex) = digest_hex(&dist.bytes);

    let filename = dist
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .context("distribution file has no filename")?
        .to_string();

    let mut form = multipart::Form::new()
        .text(":action", "file_upload")
        .text("protocol_version", "1")
        .text("metadata_version", metadata_version)
        .text("name", name.clone())
        .text("version", version.clone())
        .text("filetype", filetype)
        .text("pyversion", pyversion)
        .text("md5_digest", md5_hex)
        .text("sha256_digest", sha256_hex);

    // Optional single-valued fields, only sent when present in the source
    // metadata so we don't clobber anything on the index with blanks.
    const OPTIONAL_SINGLE: &[(&str, &str)] = &[
        ("Summary", "summary"),
        ("Home-page", "home_page"),
        ("Author", "author"),
        ("Author-email", "author_email"),
        ("Maintainer", "maintainer"),
        ("Maintainer-email", "maintainer_email"),
        ("License", "license"),
        ("Description-Content-Type", "description_content_type"),
        ("Requires-Python", "requires_python"),
        ("Keywords", "keywords"),
    ];
    for (meta_key, form_key) in OPTIONAL_SINGLE {
        if let Some(v) = dist.metadata.single.get(*meta_key) {
            form = form.text(form_key.to_string(), v.clone());
        }
    }
    if let Some(desc) = &dist.metadata.description {
        form = form.text("description", desc.clone());
    }

    const OPTIONAL_MULTI: &[(&str, &str)] = &[
        ("Classifier", "classifiers"),
        ("Requires-Dist", "requires_dist"),
        ("Project-URL", "project_urls"),
    ];
    for (meta_key, form_key) in OPTIONAL_MULTI {
        if let Some(values) = dist.metadata.multi.get(*meta_key) {
            for v in values {
                form = form.text(form_key.to_string(), v.clone());
            }
        }
    }

    let part = multipart::Part::bytes(dist.bytes.clone())
        .file_name(filename)
        .mime_str("application/octet-stream")?;
    form = form.part("content", part);

    let resp = client
        .post(repository_url)
        .basic_auth(username, Some(password))
        .multipart(form)
        .send()
        .await
        .context("sending upload request to PyPI")?;

    let status = resp.status();
    if status.is_success() {
        return Ok(true);
    }

    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 400 && body.to_lowercase().contains("already exists") {
        return Ok(false);
    }

    bail!(
        "PyPI upload of {}@{} failed: HTTP {} - {}",
        name,
        version,
        status,
        body.trim()
    );
}