use std::path::{Path, PathBuf};

use crate::modules::workspace::{resolve_path, WorkspaceEnv, WorkspaceRegistry};

const PROTECTED_DIRS: &[&str] = &[
    "/.ssh",
    "/.gnupg",
    "/.aws",
    "/.azure",
    "/.kube",
    "/.docker",
    "/.config/gh",
    "/.config/git",
    "/.config/gcloud",
    "/.config/op",
    "/.git",
    "/.terraform.d",
    "/library/keychains",
    "/library/cookies",
    "/etc",
    "/private/etc",
    "/proc",
    "/sys",
    "/var/db",
    "/var/root",
    "/private/var/db",
    "/private/var/root",
    "/appdata/roaming/microsoft/credentials",
    "/appdata/local/microsoft/credentials",
    "/appdata/roaming/gcloud",
];

const WRITE_DENY_PREFIXES: &[&str] = &[
    "/etc/",
    "/var/db/",
    "/var/root/",
    "/system/",
    "/library/keychains/",
    "/library/launchagents/",
    "/library/launchdaemons/",
    "/private/etc/",
    "/private/var/db/",
    "/usr/bin/",
    "/usr/sbin/",
    "/usr/local/bin/",
    "/bin/",
    "/sbin/",
    "/boot/",
    "/windows/",
    "/program files/",
    "/program files (x86)/",
    "/programdata/",
];

pub fn authorize_read_path(
    registry: &WorkspaceRegistry,
    path: &str,
    workspace: &WorkspaceEnv,
) -> Result<PathBuf, String> {
    check_path_text(path)?;
    check_sensitive_path(path, false)?;
    let resolved = resolve_path(path, workspace);
    let canonical =
        std::fs::canonicalize(&resolved).map_err(|e| format!("path not accessible: {e}"))?;
    require_authorized(registry, &canonical)?;
    check_sensitive_path(&canonical.to_string_lossy(), false)?;
    Ok(canonical)
}

pub fn authorize_write_path(
    registry: &WorkspaceRegistry,
    path: &str,
    workspace: &WorkspaceEnv,
) -> Result<PathBuf, String> {
    check_path_text(path)?;
    check_sensitive_path(path, true)?;
    let resolved = resolve_path(path, workspace);
    let candidate = canonical_path_by_parent(&resolved)?;
    require_authorized(registry, &candidate)?;
    check_sensitive_path(&candidate.to_string_lossy(), true)?;
    Ok(candidate)
}

fn canonical_path_by_parent(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        let parent = path
            .parent()
            .ok_or_else(|| "path has no parent".to_string())?;
        let file_name = path
            .file_name()
            .ok_or_else(|| "path has no file name".to_string())?;
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|e| format!("parent path not accessible: {e}"))?;
        return Ok(canonical_parent.join(file_name));
    }

    let mut current = path;
    let mut missing = Vec::new();
    loop {
        if current.exists() {
            let mut canonical = std::fs::canonicalize(current)
                .map_err(|e| format!("parent path not accessible: {e}"))?;
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Ok(canonical);
        }
        let Some(name) = current.file_name() else {
            return Err("no existing parent directory".into());
        };
        missing.push(name.to_os_string());
        current = current
            .parent()
            .ok_or_else(|| "no existing parent directory".to_string())?;
    }
}

fn require_authorized(registry: &WorkspaceRegistry, canonical: &Path) -> Result<(), String> {
    if registry.is_authorized(canonical) {
        Ok(())
    } else {
        Err(format!(
            "path is outside the authorized workspace: {}",
            canonical.display()
        ))
    }
}

fn check_path_text(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path is empty".into());
    }
    if path.chars().any(|c| c <= '\u{1f}') {
        return Err("path contains control characters".into());
    }
    Ok(())
}

fn check_sensitive_path(path: &str, write: bool) -> Result<(), String> {
    let base = normalized_basename(path);
    if sensitive_basename(&base) {
        return Err(format!(
            "path matches a sensitive-file pattern: {}",
            basename(path)
        ));
    }

    let cmp = comparison_form(path);
    for dir in PROTECTED_DIRS {
        if is_under_protected(&cmp, dir) {
            return Err(format!(
                "path is inside a protected directory ({})",
                dir.trim_start_matches('/')
            ));
        }
    }

    if write {
        let prefixed = if cmp.starts_with('/') {
            cmp
        } else {
            format!("/{cmp}")
        };
        for prefix in WRITE_DENY_PREFIXES {
            if prefixed.starts_with(prefix) || format!("{prefixed}/").starts_with(prefix) {
                return Err(format!(
                    "writes under {} are not allowed",
                    prefix.trim_end_matches('/')
                ));
            }
        }
    }

    Ok(())
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn normalized_basename(path: &str) -> String {
    normalize_segment(basename(path)).to_ascii_lowercase()
}

fn normalize_segment(seg: &str) -> String {
    let no_ads = seg.split(':').next().unwrap_or(seg);
    no_ads.trim_end_matches(['.', ' ']).to_string()
}

fn sensitive_basename(base: &str) -> bool {
    base == ".env"
        || base.starts_with(".env.")
        || has_sensitive_extension(base)
        || ssh_key_name(base)
        || matches!(
            base,
            "known_hosts"
                | "authorized_keys"
                | "htpasswd"
                | ".netrc"
                | "_netrc"
                | "credentials"
                | ".pgpass"
                | ".npmrc"
                | ".pypirc"
        )
        || secret_config_name(base)
        || (base.starts_with("service-account") && base.ends_with(".json"))
        || (base.starts_with("service_account") && base.ends_with(".json"))
}

fn has_sensitive_extension(base: &str) -> bool {
    [
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".asc",
        ".gpg",
        ".keystore",
        ".jks",
    ]
    .iter()
    .any(|suffix| base.ends_with(suffix))
}

fn ssh_key_name(base: &str) -> bool {
    ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|prefix| {
            base == *prefix
                || base
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with(['.', '_', '-']))
        })
}

fn secret_config_name(base: &str) -> bool {
    let Some(rest) = base
        .strip_prefix("secret.")
        .or_else(|| base.strip_prefix("secrets."))
    else {
        return false;
    };
    matches!(rest, "json" | "yaml" | "yml" | "toml" | "env")
}

fn comparison_form(path: &str) -> String {
    let mut s = path.replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = format!("/{rest}");
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic() {
        s = s[2..].to_string();
    }
    s = s
        .split('/')
        .map(normalize_segment)
        .collect::<Vec<_>>()
        .join("/");
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    s = s.to_ascii_lowercase();
    if s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

fn is_under_protected(cmp: &str, dir: &str) -> bool {
    format!("{cmp}/").contains(&format!("{dir}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn blocks_sensitive_basenames_and_protected_dirs() {
        assert!(check_sensitive_path("/tmp/.env", false).is_err());
        assert!(check_sensitive_path("/tmp/id_ed25519.bak", false).is_err());
        assert!(check_sensitive_path("/Users/me/.ssh/config", false).is_err());
        assert!(check_sensitive_path("/tmp/src/main.rs", false).is_ok());
    }

    #[test]
    fn write_path_uses_parent_not_symlink_target() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).unwrap();

        let registry = WorkspaceRegistry::default();
        registry.authorize(dir.path()).unwrap();
        let authorized =
            authorize_write_path(&registry, &link.to_string_lossy(), &WorkspaceEnv::Local)
                .expect("link path itself is inside the workspace");
        assert!(authorized.starts_with(dir.path().canonicalize().unwrap()));
    }
}
