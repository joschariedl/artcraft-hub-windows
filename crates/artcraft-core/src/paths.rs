use std::path::{Path, PathBuf};

/// All XDG locations the hub uses.
///
/// Everything is user-owned. The hub never needs root: apps go to
/// `~/.local/opt/artcraft`, launchers to `~/.local/bin` and desktop files to
/// `~/.local/share/applications`.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
    pub install_root: PathBuf,
    pub bin_dir: PathBuf,
    pub applications_dir: PathBuf,
    pub icons_dir: PathBuf,
    pub mime_dir: PathBuf,
    pub metainfo_dir: PathBuf,
    pub desktop_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
        let local_data = dirs::data_local_dir().unwrap_or_else(|| data.clone());
        let cache = dirs::cache_dir().unwrap_or_else(|| home.join(".cache"));
        let state = dirs::state_dir().unwrap_or_else(|| local_data.join("artcraft-hub/state"));
        #[cfg(target_os = "windows")]
        let (install_root, bin_dir, applications_dir) = {
            let root = local_data.join("ArtCraft");
            (root.clone(), root.join("bin"), data.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("ArtCraft"))
        };
        #[cfg(not(target_os = "windows"))]
        let (install_root, bin_dir, applications_dir) = {
            let local = home.join(".local");
            (local.join("opt/artcraft"), local.join("bin"), data.join("applications"))
        };

        Self {
            config_dir: config.join("artcraft-hub"),
            state_dir: state.join("artcraft-hub"),
            cache_dir: cache.join("artcraft-hub"),
            data_dir: data.clone(),
            install_root,
            bin_dir,
            applications_dir: applications_dir.clone(),
            icons_dir: data.join("icons"),
            mime_dir: data.join("mime"),
            metainfo_dir: data.join("metainfo"),
            desktop_dir: applications_dir,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    pub fn vt_cache_file(&self) -> PathBuf {
        self.state_dir.join("virustotal-cache.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("operation.lock")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.cache_dir.join("downloads")
    }

    /// `~/.local/state/omarchy/current/theme/colors.toml`, when Omarchy is
    /// installed. Derived from the XDG state directory so tests can redirect
    /// the whole tree.
    pub fn omarchy_theme_file(&self) -> PathBuf {
        self.omarchy_current_dir().join("theme/colors.toml")
    }

    /// `~/.local/state/omarchy/current/theme.name`.
    pub fn omarchy_theme_name_file(&self) -> PathBuf {
        self.omarchy_current_dir().join("theme.name")
    }

    fn omarchy_current_dir(&self) -> PathBuf {
        self.state_dir
            .parent()
            .map(|p| p.join("omarchy/current"))
            .unwrap_or_else(|| self.state_dir.join("omarchy/current"))
    }

    pub fn icon_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("icons")
    }

    pub fn app_dir(&self, id: &str) -> PathBuf {
        self.install_root.join(id)
    }

    pub fn app_version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.app_dir(id).join(sanitize_component(version))
    }

    pub fn desktop_file(&self, bundle: &str) -> PathBuf {
        #[cfg(target_os = "windows")]
        {
            self.desktop_dir.join(format!("{bundle}.lnk"))
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.desktop_dir.join(format!("{bundle}.desktop"))
        }
    }

    /// Where the Omarchy bar plugin lives (`~/.config/omarchy/plugins/<id>`).
    pub fn omarchy_plugins_dir(&self) -> PathBuf {
        self.config_dir
            .parent()
            .map(|p| p.join("omarchy/plugins"))
            .unwrap_or_else(|| self.config_dir.join("omarchy/plugins"))
    }

    pub fn omarchy_plugin_dir(&self) -> PathBuf {
        self.omarchy_plugins_dir().join("derfleischmann.artcraft")
    }

    /// Ensure every base directory exists.
    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [
            &self.config_dir,
            &self.state_dir,
            &self.cache_dir,
            &self.install_root,
            &self.bin_dir,
            &self.applications_dir,
            &self.icons_dir,
            &self.mime_dir,
            &self.metainfo_dir,
            &self.download_dir(),
            &self.icon_cache_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// True when `path` is inside this installation root.
    pub fn is_managed_path(&self, path: &Path) -> bool {
        path.starts_with(&self.install_root)
    }
}

/// Keep a version/tag from escaping its parent directory.
pub fn sanitize_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+') { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_traversal() {
        assert_eq!(sanitize_component("../../etc"), "_.._etc");
        assert_eq!(sanitize_component("v0.13.0"), "v0.13.0");
        assert_eq!(sanitize_component(""), "unknown");
        assert_eq!(sanitize_component(".."), "unknown");
    }
}
