#![forbid(unsafe_code)]

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

const BUILDERS: [&str; 1] = ["mmdebstrap"];
const MODES: [&str; 2] = ["app", "session"];
const LEVELS: [&str; 2] = ["standard", "reduced"];
const GPUS: [&str; 2] = ["none", "virtio-gpu-venus"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub description: String,
    pub base: Base,
    pub app: App,
    pub resources: Resources,
    pub isolation: Isolation,
    pub network: Network,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Base {
    pub builder: String,
    pub suite: String,
    pub sources: Vec<String>,
    pub packages: Vec<String>,
    #[serde(default)]
    pub firmware_include: Vec<String>,
    #[serde(default)]
    pub keyring: Option<Keyring>,
    #[serde(default)]
    pub blob: Vec<Blob>,
    #[serde(default)]
    pub file: Vec<File>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyring {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub path: String,
    pub sha256: String,
    pub into: String,
    pub strip: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub sha256: String,
    pub into: String,
    pub mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub mode: String,
    pub command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub memory_mb: u32,
    pub cpus: u32,
    pub disk_gb: u32,
    pub data_mb: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Isolation {
    pub level: String,
    pub gpu: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub via: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("не читается манифест {}", path.display()))?;
        Self::parse(&src)
    }

    pub fn parse(src: &str) -> Result<Self> {
        let manifest: Manifest =
            basic_toml::from_str(src).context("некорректный TOML манифеста")?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if !miyori_config::is_safe_slug(&self.id) {
            bail!("id {:?} не является безопасным слагом [a-z0-9-]", self.id);
        }

        // управляющий символ уходит в конфиги сборщика без экранирования и может дописать непроверенный пункт
        check_no_control(&self.description, "description")?;
        check_no_control(&self.base.builder, "base.builder")?;
        check_no_control(&self.base.suite, "base.suite")?;
        for (i, s) in self.base.sources.iter().enumerate() {
            check_no_control(s, &format!("sources[{i}]"))?;
        }
        for (i, p) in self.base.packages.iter().enumerate() {
            check_no_control(p, &format!("packages[{i}]"))?;
        }
        if let Some(k) = &self.base.keyring {
            check_no_control(&k.path, "keyring.path")?;
        }
        for (i, b) in self.base.blob.iter().enumerate() {
            check_no_control(&b.path, &format!("blob[{i}].path"))?;
            check_no_control(&b.into, &format!("blob[{i}].into"))?;
        }
        for (i, f) in self.base.file.iter().enumerate() {
            check_no_control(&f.path, &format!("file[{i}].path"))?;
            check_no_control(&f.into, &format!("file[{i}].into"))?;
        }
        check_no_control(&self.app.mode, "app.mode")?;
        check_no_control(&self.app.command, "app.command")?;
        check_no_control(&self.isolation.level, "isolation.level")?;
        check_no_control(&self.isolation.gpu, "isolation.gpu")?;
        check_no_control(&self.isolation.reason, "isolation.reason")?;
        check_no_control(&self.network.via, "network.via")?;

        if !BUILDERS.contains(&self.base.builder.as_str()) {
            bail!(
                "builder {:?} неизвестен; реализован только {:?}",
                self.base.builder,
                BUILDERS[0]
            );
        }
        // suite идёт позиционным аргументом, а Getopt::Long переставляет аргументы
        if !miyori_config::is_safe_slug(&self.base.suite) {
            bail!(
                "suite {:?} не является безопасным слагом [a-z0-9-]",
                self.base.suite
            );
        }
        if self.base.packages.is_empty() {
            bail!("packages пуст: собирать нечего");
        }
        if self.base.sources.is_empty() {
            bail!("sources пуст: mmdebstrap неоткуда брать пакеты");
        }
        for line in &self.base.sources {
            if !line.starts_with("deb ") || !line.contains("://") {
                bail!(
                    "sources: строка {:?} не похожа на строку apt-источника",
                    line
                );
            }
        }
        for f in &self.base.firmware_include {
            // задача 4 подставляет значение в dpkg path-include без экранирования
            if !is_firmware_slug(f) {
                bail!(
                    "firmware_include: {:?} не является безопасным слагом [a-z0-9_-]",
                    f
                );
            }
        }
        if !MODES.contains(&self.app.mode.as_str()) {
            bail!("mode {:?} неизвестен, допустимы {:?}", self.app.mode, MODES);
        }
        if !self.app.command.starts_with('/') {
            bail!(
                "command {:?} должен быть абсолютным путём",
                self.app.command
            );
        }
        if !LEVELS.contains(&self.isolation.level.as_str()) {
            bail!(
                "level {:?} неизвестен, допустимы {:?}",
                self.isolation.level,
                LEVELS
            );
        }
        if !GPUS.contains(&self.isolation.gpu.as_str()) {
            bail!(
                "gpu {:?} неизвестен, допустимы {:?}",
                self.isolation.gpu,
                GPUS
            );
        }
        // отдать гостю графический стек хоста можно только там, где это названо вслух
        if self.isolation.gpu != "none" && self.isolation.level != "reduced" {
            bail!(
                "gpu {:?} требует level = \"reduced\", а указан {:?}",
                self.isolation.gpu,
                self.isolation.level
            );
        }
        if self.isolation.level == "reduced" && self.isolation.reason.trim().is_empty() {
            bail!("level = \"reduced\" требует непустой reason: он показывается пользователю");
        }
        if self.network.via != "miyori-net" {
            bail!(
                "via {:?}: единственный выход наружу — miyori-net",
                self.network.via
            );
        }
        if self.resources.memory_mb < 256 || self.resources.cpus < 1 || self.resources.disk_gb < 1 {
            bail!("resources ниже разумного минимума: 256 МБ, 1 CPU, 1 ГБ диска");
        }
        if let Some(k) = &self.base.keyring {
            check_sha256(&k.sha256, "keyring")?;
        }
        for b in &self.base.blob {
            check_sha256(&b.sha256, &b.path)?;
            if !b.into.starts_with('/') {
                bail!("blob {:?}: into должен быть абсолютным путём", b.path);
            }
            // ".." в into выводит распаковку тарбола из дерева сборки на хостовую ФС
            if b.into.split('/').any(|c| c == "..") {
                bail!(
                    "blob {:?}: into {:?} содержит компонент \"..\"",
                    b.path,
                    b.into
                );
            }
        }
        for f in &self.base.file {
            check_sha256(&f.sha256, &f.path)?;
            if !f.into.starts_with('/') {
                bail!("file {:?}: into должен быть абсолютным путём", f.path);
            }
            if f.into.split('/').any(|c| c == "..") {
                bail!(
                    "file {:?}: into {:?} содержит компонент \"..\"",
                    f.path,
                    f.into
                );
            }
            check_mode(&f.mode, &f.path)?;
        }
        Ok(())
    }
}

fn is_firmware_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn check_no_control(value: &str, field: &str) -> Result<()> {
    if value.chars().any(|c| c.is_control()) {
        bail!(
            "{field}: значение содержит управляющий символ, недопустимо: {:?}",
            value
        );
    }
    Ok(())
}

fn check_sha256(value: &str, what: &str) -> Result<()> {
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "{what}: sha256 должен быть 64 hex-символа, получено {:?}",
            value
        );
    }
    Ok(())
}

fn check_mode(value: &str, what: &str) -> Result<()> {
    if value.len() != 4 || !value.chars().all(|c| ('0'..='7').contains(&c)) {
        bail!(
            "{what}: mode должен быть ровно 4 восьмеричные цифры, получено {:?}",
            value
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard() -> String {
        r#"
        id = "telegram"
        description = "Telegram Desktop"

        [base]
        builder = "mmdebstrap"
        suite = "noble"
        sources = ["deb http://archive.ubuntu.com/ubuntu noble main universe"]
        packages = ["telegram-desktop"]

        [app]
        mode = "app"
        command = "/usr/bin/telegram-desktop"

        [resources]
        memory_mb = 1024
        cpus = 1
        disk_gb = 8
        data_mb = 4096

        [isolation]
        level = "standard"
        gpu = "none"

        [network]
        via = "miyori-net"
        "#
        .to_string()
    }

    #[test]
    fn standard_manifest_parses() {
        let m = Manifest::parse(&standard()).unwrap();
        assert_eq!(m.id, "telegram");
        assert_eq!(m.app.mode, "app");
        assert_eq!(m.resources.memory_mb, 1024);
        assert!(m.base.blob.is_empty());
        assert!(m.base.file.is_empty());
    }

    #[test]
    fn gpu_without_reduced_level_is_rejected() {
        let src = standard().replace(r#"gpu = "none""#, r#"gpu = "virtio-gpu-venus""#);
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("reduced"),
            "сообщение должно называть уровень: {err}"
        );
    }

    #[test]
    fn reduced_without_reason_is_rejected() {
        let src = standard().replace(r#"level = "standard""#, r#"level = "reduced""#);
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("reason"),
            "сообщение должно называть поле: {err}"
        );
    }

    #[test]
    fn reduced_with_reason_and_gpu_parses() {
        let src = standard()
            .replace(r#"level = "standard""#, r#"level = "reduced""#)
            .replace(
                r#"gpu = "none""#,
                "gpu = \"virtio-gpu-venus\"\nreason = \"полноэкранная графика\"",
            );
        let m = Manifest::parse(&src).unwrap();
        assert_eq!(m.isolation.level, "reduced");
    }

    #[test]
    fn unknown_builder_is_rejected() {
        let src = standard().replace(r#"builder = "mmdebstrap""#, r#"builder = "pacstrap""#);
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn unknown_field_is_rejected_not_ignored() {
        let src = standard().replace("[app]", "sneaky = true\n\n[app]");
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn relative_command_is_rejected() {
        let src = standard().replace(
            r#"command = "/usr/bin/telegram-desktop""#,
            r#"command = "telegram-desktop""#,
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn network_other_than_miyori_net_is_rejected() {
        let src = standard().replace(r#"via = "miyori-net""#, r#"via = "direct""#);
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn id_must_be_safe_slug() {
        let src = standard().replace(r#"id = "telegram""#, r#"id = "../etc/passwd""#);
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn blob_sha256_must_be_64_hex() {
        let src = format!(
            "{}\n[[base.blob]]\npath = \"blobs/x.tar.gz\"\nsha256 = \"deadbeef\"\ninto = \"/opt/x\"\nstrip = 1\n",
            standard()
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn blob_into_must_be_absolute() {
        let src = format!(
            "{}\n[[base.blob]]\npath = \"blobs/x.tar.gz\"\nsha256 = \"{}\"\ninto = \"opt/x\"\nstrip = 1\n",
            standard(),
            "a".repeat(64)
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn blob_into_with_dotdot_is_rejected() {
        // PoC ревьюера: into = "/../../../../../../tmp/..." распаковывал тарбол вне дерева сборки
        let src = format!(
            "{}\n[[base.blob]]\npath = \"blobs/x.tar.gz\"\nsha256 = \"{}\"\ninto = \"/../../../../../../tmp/pwned\"\nstrip = 1\n",
            standard(),
            "a".repeat(64)
        );
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("into"),
            "сообщение должно называть поле: {err}"
        );
    }

    #[test]
    fn blob_into_with_embedded_dotdot_is_rejected() {
        // не только префикс: ".." может стоять и в середине пути
        let src = format!(
            "{}\n[[base.blob]]\npath = \"blobs/x.tar.gz\"\nsha256 = \"{}\"\ninto = \"/opt/../../etc\"\nstrip = 1\n",
            standard(),
            "a".repeat(64)
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn file_sha256_must_be_64_hex() {
        let src = format!(
            "{}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"deadbeef\"\ninto = \"/opt/x\"\nmode = \"0755\"\n",
            standard()
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn file_into_must_be_absolute() {
        let src = format!(
            "{}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"{}\"\ninto = \"opt/x\"\nmode = \"0755\"\n",
            standard(),
            "a".repeat(64)
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn file_into_with_dotdot_is_rejected() {
        let src = format!(
            "{}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"{}\"\ninto = \"/../../../../../../tmp/pwned\"\nmode = \"0755\"\n",
            standard(),
            "a".repeat(64)
        );
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("into"),
            "сообщение должно называть поле: {err}"
        );
    }

    #[test]
    fn file_mode_must_be_four_octal_digits() {
        for bad in ["755", "07555", "0788", "07x5"] {
            let src = format!(
                "{}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"{}\"\ninto = \"/opt/x\"\nmode = \"{}\"\n",
                standard(),
                "a".repeat(64),
                bad
            );
            assert!(
                Manifest::parse(&src).is_err(),
                "{bad:?} должен быть отвергнут"
            );
        }
    }

    #[test]
    fn manifest_without_file_still_parses() {
        let m = Manifest::parse(&standard()).unwrap();
        assert!(m.base.file.is_empty());
    }

    #[test]
    fn file_with_valid_mode_parses() {
        let src = format!(
            "{}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"{}\"\ninto = \"/opt/x\"\nmode = \"0755\"\n",
            standard(),
            "a".repeat(64)
        );
        let m = Manifest::parse(&src).unwrap();
        assert_eq!(m.base.file.len(), 1);
        assert_eq!(m.base.file[0].mode, "0755");
    }

    #[test]
    fn suite_must_be_safe_slug() {
        // PoC ревьюера: "--logfile=<путь>" в позиции SUITE mmdebstrap разбирает как опцию
        let src = standard().replace(r#"suite = "noble""#, r#"suite = "--logfile=/etc/pwned""#);
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("suite"),
            "сообщение должно называть поле: {err}"
        );
    }

    #[test]
    fn suite_empty_is_rejected() {
        let src = standard().replace(r#"suite = "noble""#, r#"suite = """#);
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn empty_packages_is_rejected() {
        let src = standard().replace(r#"packages = ["telegram-desktop"]"#, "packages = []");
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn unknown_mode_is_rejected() {
        let src = standard().replace(r#"mode = "app""#, r#"mode = "kiosk""#);
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn empty_sources_is_rejected() {
        let src = standard().replace(
            r#"sources = ["deb http://archive.ubuntu.com/ubuntu noble main universe"]"#,
            "sources = []",
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn source_not_looking_like_apt_line_is_rejected() {
        let src = standard().replace(
            r#"sources = ["deb http://archive.ubuntu.com/ubuntu noble main universe"]"#,
            r#"sources = ["archive.ubuntu.com/ubuntu noble main universe"]"#,
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn resources_below_minimum_is_rejected() {
        for (from, to) in [
            (r#"memory_mb = 1024"#, r#"memory_mb = 255"#),
            (r#"cpus = 1"#, r#"cpus = 0"#),
            (r#"disk_gb = 8"#, r#"disk_gb = 0"#),
        ] {
            let src = standard().replace(from, to);
            assert!(Manifest::parse(&src).is_err(), "{to} должен быть отвергнут");
        }
    }

    #[test]
    fn unknown_gpu_is_rejected() {
        // level = "reduced" уже сам по себе валиден, чтобы отказ шёл именно из-за белого списка gpu
        let src = standard()
            .replace(r#"level = "standard""#, r#"level = "reduced""#)
            .replace(
                r#"gpu = "none""#,
                "gpu = \"nvidia-full-passthrough\"\nreason = \"тест\"",
            );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn keyring_sha256_must_be_64_hex() {
        let src = format!(
            "{}\n[base.keyring]\npath = \"keys/kali.gpg\"\nsha256 = \"deadbeef\"\n",
            standard()
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn firmware_include_must_be_safe_slug() {
        let src = standard().replace(
            r#"packages = ["telegram-desktop"]"#,
            "packages = [\"telegram-desktop\"]\nfirmware_include = [\"../etc\"]",
        );
        assert!(Manifest::parse(&src).is_err());
    }

    #[test]
    fn control_char_in_source_is_rejected() {
        // TOML basic-string \n даёт второй, непроверенный apt-источник в одной строке
        let src = standard().replace(
            r#"sources = ["deb http://archive.ubuntu.com/ubuntu noble main universe"]"#,
            "sources = [\"deb http://archive.ubuntu.com/ubuntu noble main\\ndeb [trusted=yes] http://evil.example.com/repo ./\"]",
        );
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("sources"),
            "сообщение должно называть поле: {err}"
        );
    }

    #[test]
    fn control_char_in_command_is_rejected() {
        let src = standard().replace(
            r#"command = "/usr/bin/telegram-desktop""#,
            r#"command = "/bin/true\nPWNED=ignored""#,
        );
        let err = Manifest::parse(&src).unwrap_err().to_string();
        assert!(
            err.contains("command"),
            "сообщение должно называть поле: {err}"
        );
    }
}
