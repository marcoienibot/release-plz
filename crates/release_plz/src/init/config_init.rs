use std::{io::Write, path::Path};

use anyhow::Context as _;

const DEFAULT_CONFIG: &str = "# Configuration: https://release-plz.dev/docs/config\n[workspace]\n";

/// Use an empty workspace section so omitted options keep their release-plz defaults.
pub fn create_default_config(workspace_root: &Path) -> anyhow::Result<()> {
    for name in ["release-plz.toml", ".release-plz.toml"] {
        let path = workspace_root.join(name);
        if path.try_exists()? {
            println!("Config file already exists at {}", path.display());
            return Ok(());
        }
    }
    let path = workspace_root.join("release-plz.toml");
    // Also protect against overwriting a config created after the existence check.
    let mut file = fs_err::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .context("failed to create release-plz configuration")?;
    file.write_all(DEFAULT_CONFIG.as_bytes())
        .context("failed to write release-plz configuration")?;
    println!("Created config file at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_init_writes_parseable_defaults_in_requested_directory() {
        let directory = tempfile::tempdir().unwrap();
        create_default_config(directory.path()).unwrap();
        let content = fs_err::read_to_string(directory.path().join("release-plz.toml")).unwrap();
        let config: crate::config::Config = toml::from_str(&content).unwrap();
        assert_eq!(config, crate::config::Config::default());
    }

    #[test]
    fn config_init_preserves_existing_configuration() {
        for name in ["release-plz.toml", ".release-plz.toml"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(name);
            let content = "[workspace]\npr_draft = true\n";
            fs_err::write(&path, content).unwrap();
            create_default_config(directory.path()).unwrap();
            assert_eq!(fs_err::read_to_string(&path).unwrap(), content);
            if name.starts_with('.') {
                assert!(!directory.path().join("release-plz.toml").exists());
            }
        }
    }
}
