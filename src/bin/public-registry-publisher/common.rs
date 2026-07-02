use std::path::{Path, PathBuf};

/// Parse a comma-separated SYNC_PACKAGES value into trimmed, non-empty entries.
pub fn parse_package_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Find files in `dir` whose (hyphen/underscore-normalized) filename starts with
/// `pkg` followed by a version-looking separator ('-' or '_'). This mirrors the
/// loose prefix match the bash entrypoint used, so PACKAGES_DIR contents laid
/// out by pypiserver (e.g. `ginger_dj_framework-1.2.3.tar.gz`,
/// `ginger_dj_framework-1.2.3-py3-none-any.whl`) still match regardless of the
/// hyphen/underscore normalization PyPI applies to distribution filenames.
///
/// Only `.whl` and `.tar.gz` files are considered; pypiserver's `.metadata.json`
/// sidecar files are always skipped.
pub fn find_py_matches(dir: &Path, pkg: &str) -> anyhow::Result<Vec<PathBuf>> {
    let normalized_pkg = pkg.replace('-', "_");
    let mut out = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let base = match path.file_name().and_then(|n| n.to_str()) {
            Some(b) => b,
            None => continue,
        };
        if !(base.ends_with(".whl") || base.ends_with(".tar.gz")) {
            continue;
        }
        let base_normalized = base.replace('-', "_");
        if base_normalized.starts_with(&format!("{}_", normalized_pkg)) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}