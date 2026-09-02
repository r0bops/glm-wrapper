use std::env;

use anyhow::{bail, Result};

/// A resolved API key and where it came from.
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    pub value: String,
    pub source: KeySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    GlmEnv,
    ZaiEnv,
    File,
}

impl KeySource {
    pub fn describe(self) -> String {
        match self {
            KeySource::GlmEnv => "GLM_API_KEY environment variable".to_string(),
            KeySource::ZaiEnv => "ZAI_API_KEY environment variable".to_string(),
            KeySource::File => crate::paths::key_file().display().to_string(),
        }
    }
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.describe())
    }
}

/// Resolution order: GLM_API_KEY > ZAI_API_KEY > ~/.config/glm/key (trimmed).
pub fn resolve_key(file: Option<&std::path::Path>) -> Result<ResolvedKey> {
    if let Some(v) = env::var("GLM_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Ok(ResolvedKey {
            value: v,
            source: KeySource::GlmEnv,
        });
    }
    if let Some(v) = env::var("ZAI_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Ok(ResolvedKey {
            value: v,
            source: KeySource::ZaiEnv,
        });
    }
    let path = file
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(crate::paths::key_file);
    if let Some(v) = crate::util::read_secret(&path)? {
        return Ok(ResolvedKey {
            value: v,
            source: KeySource::File,
        });
    }
    bail!("{}", missing_key_hint())
}

/// The literal options line printed by `glm init` when no key resolves.
pub fn missing_key_hint() -> String {
    "no API key found. Provide one with:\n  \
     GLM_API_KEY=... glm ...      (or ZAI_API_KEY)\n  \
     glm key set                  (stores in ~/.config/glm/key)\n  \
     glm init                     (first-run setup)"
        .to_string()
}

pub fn store_key(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty API key");
    }
    crate::util::write_secret(&crate::paths::key_file(), &format!("{value}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_key_file(content: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("glm-keys-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key");
        match content {
            Some(c) => crate::util::write_secret(&file, c).unwrap(),
            None => {
                std::fs::remove_file(&file).ok();
            }
        }
        file
    }

    #[test]
    fn order_glm_zai_file() {
        let _g = ENV_LOCK.lock().unwrap();
        env::remove_var("GLM_API_KEY");
        env::remove_var("ZAI_API_KEY");
        let f = temp_key_file(Some("file-key\n"));

        env::set_var("ZAI_API_KEY", "zai-key");
        let r = resolve_key(Some(&f)).unwrap();
        assert_eq!(r.value, "zai-key");
        assert_eq!(r.source, KeySource::ZaiEnv);

        env::set_var("GLM_API_KEY", "glm-key");
        let r = resolve_key(Some(&f)).unwrap();
        assert_eq!(r.value, "glm-key");
        assert_eq!(r.source, KeySource::GlmEnv);
    }

    #[test]
    fn file_key_is_trimmed() {
        let _g = ENV_LOCK.lock().unwrap();
        env::remove_var("GLM_API_KEY");
        env::remove_var("ZAI_API_KEY");
        let f = temp_key_file(Some("  sk-trimmed \n\n"));
        let r = resolve_key(Some(&f)).unwrap();
        assert_eq!(r.value, "sk-trimmed");
        assert_eq!(r.source, KeySource::File);
    }

    #[test]
    fn missing_key_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        env::remove_var("GLM_API_KEY");
        env::remove_var("ZAI_API_KEY");
        let f = temp_key_file(None);
        let err = resolve_key(Some(&f)).unwrap_err().to_string();
        assert!(err.contains("GLM_API_KEY"), "err: {err}");
        assert!(err.contains("key set"), "err: {err}");
        assert!(err.contains("init"), "err: {err}");
    }
}
