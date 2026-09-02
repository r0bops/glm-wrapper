use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Write `body` to `path` atomically (temp file in the same dir + rename).
pub fn atomic_write(path: &Path, body: &str) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Create `dir` with 0700, then `file` inside it with 0600.
pub fn write_secret(file: &Path, content: &str) -> anyhow::Result<()> {
    let dir = file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("secret path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    // Create with 0600 from the start so the key is never world-readable,
    // not even between create and chmod.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Read a secret file, trimming surrounding whitespace. Returns None if missing.
pub fn read_secret(file: &Path) -> anyhow::Result<Option<String>> {
    if !file.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(file)?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Prompt on the controlling tty without echo. Echo is disabled with
/// `stty -echo` on /dev/tty for the duration of the read and always restored,
/// even on error. If /dev/tty is unavailable (CI, pipes) we read one line
/// from stdin instead.
pub fn prompt_secret(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, BufReader};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty");
    let Ok(tty) = tty else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    };

    let stty = |arg: &str| {
        let Ok(stdin) = std::fs::File::open("/dev/tty") else {
            return;
        };
        let _ = std::process::Command::new("stty")
            .arg(arg)
            .stdin(stdin)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };

    struct EchoGuard<F: Fn(&str)>(F);
    impl<F: Fn(&str)> Drop for EchoGuard<F> {
        fn drop(&mut self) {
            (self.0)("echo");
            eprintln!();
        }
    }

    stty("-echo");
    let _guard = EchoGuard(stty);
    let mut line = String::new();
    BufReader::new(tty).read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_round_trip_with_trim() {
        let dir = std::env::temp_dir().join(format!("glm-secret-test-{}", std::process::id()));
        let file = dir.join("key");
        write_secret(&file, "  sk-abc123  \n").unwrap();
        let got = read_secret(&file).unwrap().unwrap();
        assert_eq!(got, "sk-abc123");
        // permissions are restricted
        let meta = std::fs::metadata(&file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_secret_is_none() {
        let file = std::env::temp_dir().join(format!("glm-secret-missing-{}", std::process::id()));
        assert!(read_secret(&file).unwrap().is_none());
    }

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join(format!("glm-atomic-{}", std::process::id()));
        let file = dir.join("out.txt");
        atomic_write(&file, "one").unwrap();
        atomic_write(&file, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        std::fs::remove_dir_all(&dir).ok();
    }
}
