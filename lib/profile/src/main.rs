#![forbid(unsafe_code)]

use anyhow::{bail, Result};
use miyori_profile::Manifest;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("export-env") => {
            let path: PathBuf = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("export-env требует путь к манифесту"))?;
            print!("{}", export_env(&Manifest::load(&path)?));
            Ok(())
        }
        other => bail!("неизвестная подкоманда {:?}, есть только export-env", other),
    }
}

fn export_env(m: &Manifest) -> String {
    let mut out = String::new();
    let mut put = |k: &str, v: &str| out.push_str(&format!("{k}={}\n", shell_quote(v)));

    put("MIYORI_ID", &m.id);
    put("MIYORI_BUILDER", &m.base.builder);
    put("MIYORI_SUITE", &m.base.suite);
    // запятая — формат mmdebstrap --include, а не наш выбор
    put("MIYORI_PACKAGES", &m.base.packages.join(","));
    put("MIYORI_FIRMWARE", &m.base.firmware_include.join(" "));
    put("MIYORI_SOURCE_COUNT", &m.base.sources.len().to_string());
    for (i, line) in m.base.sources.iter().enumerate() {
        put(&format!("MIYORI_SOURCE_{i}"), line);
    }
    put("MIYORI_MODE", &m.app.mode);
    put("MIYORI_COMMAND", &m.app.command);
    put("MIYORI_MEMORY_MB", &m.resources.memory_mb.to_string());
    put("MIYORI_CPUS", &m.resources.cpus.to_string());
    put("MIYORI_DISK_GB", &m.resources.disk_gb.to_string());
    put("MIYORI_DATA_MB", &m.resources.data_mb.to_string());
    put("MIYORI_LEVEL", &m.isolation.level);
    put("MIYORI_GPU", &m.isolation.gpu);

    let (kp, ks) = match &m.base.keyring {
        Some(k) => (k.path.as_str(), k.sha256.as_str()),
        None => ("", ""),
    };
    put("MIYORI_KEYRING_PATH", kp);
    put("MIYORI_KEYRING_SHA256", ks);

    put("MIYORI_BLOB_COUNT", &m.base.blob.len().to_string());
    for (i, b) in m.base.blob.iter().enumerate() {
        put(&format!("MIYORI_BLOB_{i}_PATH"), &b.path);
        put(&format!("MIYORI_BLOB_{i}_SHA256"), &b.sha256);
        put(&format!("MIYORI_BLOB_{i}_INTO"), &b.into);
        put(&format!("MIYORI_BLOB_{i}_STRIP"), &b.strip.to_string());
    }

    put("MIYORI_FILE_COUNT", &m.base.file.len().to_string());
    for (i, f) in m.base.file.iter().enumerate() {
        put(&format!("MIYORI_FILE_{i}_PATH"), &f.path);
        put(&format!("MIYORI_FILE_{i}_SHA256"), &f.sha256);
        put(&format!("MIYORI_FILE_{i}_INTO"), &f.into);
        put(&format!("MIYORI_FILE_{i}_MODE"), &f.mode);
    }
    out
}

// вывод идёт в eval, поэтому одинарная кавычка внутри значения обязана быть закрыта и открыта заново
fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', r"'\''"))
}
