#![forbid(unsafe_code)]

use crate::render::MIYORI_NET_ID;
use anyhow::{bail, Context, Result};
use miyori_proto::ids::{Color, Label, SpaceId};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const UID_BASE: u32 = 70000;
const MAX_CID: u32 = 255;
// 0-2 зарезервированы ядром (hypervisor/local/host) — исключены тем, что перебор ниже начинается с 3
const NET_CID: u32 = 9;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceConfig {
    pub id: SpaceId,
    pub profile: SpaceId,
    pub digest: String,
    pub cid: u32,
    pub uid: u32,
    pub label: Label,
    pub color: Color,
    pub created: String,
    // None — ещё ни разу не останавливали; переживает рестарт демона, run-dir на это не годится
    #[serde(default)]
    pub clean_shutdown: Option<bool>,
    // выбирается один раз при create и не меняется; старый config.toml без поля — не зашифрован (ADR-9)
    #[serde(default)]
    pub encrypted: bool,
}

pub struct Store {
    state_dir: PathBuf,
    passwd_path: PathBuf,
    // /sys/class/net в бою; тест подставляет свой каталог, иначе занятость tap не проверить
    net_dir: PathBuf,
    spaces: Vec<SpaceConfig>,
}

impl Store {
    pub fn load(state_dir: &Path) -> Result<Self> {
        Self::load_with_passwd(state_dir, Path::new("/etc/passwd"))
    }

    pub fn load_with_passwd(state_dir: &Path, passwd_path: &Path) -> Result<Self> {
        Self::load_with_paths(state_dir, passwd_path, Path::new("/sys/class/net"))
    }

    pub fn load_with_paths(state_dir: &Path, passwd_path: &Path, net_dir: &Path) -> Result<Self> {
        let spaces_dir = state_dir.join("spaces");
        let mut spaces = Vec::new();
        if spaces_dir.exists() {
            for entry in std::fs::read_dir(&spaces_dir)
                .with_context(|| format!("не читается {}", spaces_dir.display()))?
            {
                let entry = entry.context("не читается запись каталога spaces")?;
                let config_path = entry.path().join("config.toml");
                // каталог без config.toml — не наш случай (например мусор от прежнего прогона)
                if !config_path.is_file() {
                    continue;
                }
                let src = std::fs::read_to_string(&config_path)
                    .with_context(|| format!("не читается {}", config_path.display()))?;
                let config: SpaceConfig = basic_toml::from_str(&src)
                    .with_context(|| format!("некорректный TOML {}", config_path.display()))?;
                spaces.push(config);
            }
        }
        spaces.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            passwd_path: passwd_path.to_path_buf(),
            net_dir: net_dir.to_path_buf(),
            spaces,
        })
    }

    pub fn list(&self) -> &[SpaceConfig] {
        &self.spaces
    }

    pub fn get(&self, id: &SpaceId) -> Option<&SpaceConfig> {
        self.spaces.iter().find(|s| &s.id == id)
    }

    pub fn create_space(
        &mut self,
        id: SpaceId,
        profile: SpaceId,
        digest: String,
        label: Label,
        color: Color,
        encrypted: bool,
    ) -> Result<&SpaceConfig> {
        if id.as_ref() == MIYORI_NET_ID {
            bail!("id \"{MIYORI_NET_ID}\" зарезервирован за сетевой VM");
        }
        if self.get(&id).is_some() {
            bail!("спейс \"{id}\" уже существует");
        }

        let cid = self.allocate_cid()?;
        let uid = UID_BASE + cid;
        ensure_uid_is_free(&self.passwd_path, uid)?;

        let config = SpaceConfig {
            id: id.clone(),
            profile,
            digest,
            cid,
            uid,
            label,
            color,
            created: now_rfc3339(),
            clean_shutdown: None,
            encrypted,
        };

        let dir = self.space_dir(&config.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;
        write_config_atomically(&dir, &config)?;

        self.spaces.push(config);
        // тот же порядок, что и после load(): иначе рендер реестра зависел бы от истории процесса
        self.spaces.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
        let pos = self
            .spaces
            .iter()
            .position(|s| s.id == id)
            .expect("только что добавили элемент");
        Ok(&self.spaces[pos])
    }

    // вызывается только из ops::stop; пишется всегда, а не только при отказе — молчание есть тоже ответ
    pub fn set_clean_shutdown(&mut self, id: &SpaceId, clean: bool) -> Result<()> {
        let pos = self
            .spaces
            .iter()
            .position(|s| &s.id == id)
            .with_context(|| format!("спейс \"{id}\" не найден"))?;
        self.spaces[pos].clean_shutdown = Some(clean);
        write_config_atomically(&self.space_dir(id), &self.spaces[pos])
    }

    // вызывается только из ops::update_image, тем же приёмом, что set_clean_shutdown
    pub fn set_digest(&mut self, id: &SpaceId, digest: String) -> Result<()> {
        let pos = self
            .spaces
            .iter()
            .position(|s| &s.id == id)
            .with_context(|| format!("спейс \"{id}\" не найден"))?;
        self.spaces[pos].digest = digest;
        write_config_atomically(&self.space_dir(id), &self.spaces[pos])
    }

    pub fn remove_space(&mut self, id: &SpaceId) -> Result<()> {
        let pos = self
            .spaces
            .iter()
            .position(|s| &s.id == id)
            .with_context(|| format!("спейс \"{id}\" не найден"))?;
        let dir = self.space_dir(id);
        std::fs::remove_dir_all(&dir).with_context(|| format!("не удалить {}", dir.display()))?;
        self.spaces.remove(pos);
        Ok(())
    }

    fn space_dir(&self, id: &SpaceId) -> PathBuf {
        self.state_dir.join("spaces").join(id.as_ref())
    }

    fn allocate_cid(&self) -> Result<u32> {
        'candidate: for cid in 3..=MAX_CID {
            if cid == NET_CID {
                continue;
            }
            for space in &self.spaces {
                if space.cid == cid {
                    continue 'candidate;
                }
            }
            // CID уникален на весь хост: он же адрес vsock, он же имя tap, и занять чужой
            // значит отобрать порт у соседа или упереться в EEXIST уже на старте
            if self.net_dir.join(crate::space_tap(cid)).exists() {
                continue;
            }
            return Ok(cid);
        }
        bail!("нет свободных CID в диапазоне 3..={MAX_CID}")
    }
}

fn ensure_uid_is_free(passwd_path: &Path, uid: u32) -> Result<()> {
    let src = std::fs::read_to_string(passwd_path)
        .with_context(|| format!("не читается {}", passwd_path.display()))?;
    let uid_str = uid.to_string();
    for line in src.lines() {
        if line.split(':').nth(2) == Some(uid_str.as_str()) {
            bail!("uid {uid} уже занят в {}", passwd_path.display());
        }
    }
    Ok(())
}

fn write_config_atomically(dir: &Path, config: &SpaceConfig) -> Result<()> {
    let toml = basic_toml::to_string(config).context("не удалось сериализовать config.toml")?;
    let final_path = dir.join("config.toml");
    let tmp_path = dir.join("config.toml.tmp");
    std::fs::write(&tmp_path, toml)
        .with_context(|| format!("не удалось записать {}", tmp_path.display()))?;
    // план требует ровно 0644; umask оператора (например 002) даёт 0664 — на него полагаться нельзя
    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("не удалось выставить права на {}", tmp_path.display()))?;
    // rename в пределах каталога атомарен: краш между write и rename не оставит частичный config.toml
    std::fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "не удалось переименовать {} в {}",
            tmp_path.display(),
            final_path.display()
        )
    })
}

// без chrono (новые зависимости — решение оператора): ISO-8601 UTC по алгоритму Хауарда Хиннанта
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    rfc3339_from_epoch(secs)
}

// чистая функция отдельно от SystemTime::now() — иначе календарную арифметику нечем тестировать
fn rfc3339_from_epoch(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-store-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // общий passwd для тестов, которым не важно его содержимое
    // каталог сетевых интерфейсов свой: иначе выдача CID зависела бы от того, какие tap
    // подняты на машине прямо сейчас, и тест мерил бы состояние хоста, а не код
    fn store(dir: &Path) -> Store {
        let passwd = dir.join("passwd");
        if !passwd.exists() {
            std::fs::write(&passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        }
        let net = dir.join("net");
        std::fs::create_dir_all(&net).unwrap();
        Store::load_with_paths(dir, &passwd, &net).unwrap()
    }

    fn id(s: &str) -> SpaceId {
        SpaceId::new(s).unwrap()
    }

    fn label() -> Label {
        Label::new("untrusted").unwrap()
    }

    fn color() -> Color {
        Color::new("#e03131").unwrap()
    }

    fn digest() -> String {
        "d".repeat(64)
    }

    #[test]
    fn allocates_lowest_free_cid_from_three() {
        let dir = temp_dir("lowest-cid");
        let mut s = store(&dir);
        let cfg = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        assert_eq!(cfg.cid, 3);
    }

    #[test]
    fn never_allocates_reserved_cids() {
        let dir = temp_dir("reserved-cids");
        let mut s = store(&dir);
        for name in ["a", "b", "c", "e"] {
            let cfg = s
                .create_space(id(name), id("spike"), digest(), label(), color(), false)
                .unwrap();
            assert!(
                ![0, 1, 2, 9].contains(&cfg.cid),
                "cid {} зарезервирован",
                cfg.cid
            );
        }
    }

    #[test]
    fn reuses_cid_freed_by_destroy() {
        let dir = temp_dir("reuse-cid");
        let mut s = store(&dir);
        let cid = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap()
            .cid;
        s.remove_space(&id("a")).unwrap();
        let cfg2 = s
            .create_space(id("b"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        assert_eq!(cfg2.cid, cid);
    }

    #[test]
    fn uid_is_derived_from_cid() {
        let dir = temp_dir("uid-derived");
        let mut s = store(&dir);
        let cfg = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        assert_eq!(cfg.uid, 70000 + cfg.cid);
    }

    #[test]
    fn refuses_uid_already_present_in_passwd() {
        let dir = temp_dir("uid-taken");
        let passwd = dir.join("passwd");
        // cid 3 -> uid 70003, притворяемся, что он уже кем-то занят
        std::fs::write(&passwd, "someone:x:70003:70003::/home/someone:/bin/false\n").unwrap();
        // каталог сети обязан быть пустым и своим: с настоящим /sys/class/net поднятая оснастка
        // заняла бы cid 3 и 4, спейс получил бы uid 70005, и тест бы молча проверял не то
        let net = dir.join("net");
        std::fs::create_dir_all(&net).unwrap();
        let mut s = Store::load_with_paths(&dir, &passwd, &net).unwrap();
        let err = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap_err();
        assert!(err.to_string().contains("70003"), "{err}");
    }

    // находка прогона 71-lifecycle.sh: демон выдал CID 3, чей tap уже держала фикстура,
    // а уборка теста снесла чужой интерфейс со стенда
    #[test]
    fn skips_cid_whose_tap_already_exists_on_the_host() {
        let dir = temp_dir("busy-tap");
        let passwd = dir.join("passwd");
        std::fs::write(&passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        let net = dir.join("net");
        std::fs::create_dir_all(net.join("tap-space-3")).unwrap();
        std::fs::create_dir_all(net.join("tap-space-4")).unwrap();
        let mut s = Store::load_with_paths(&dir, &passwd, &net).unwrap();
        let cfg = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        assert_eq!(cfg.cid, 5, "CID 3 и 4 заняты чужими tap, 9 зарезервирован");
    }

    #[test]
    fn refuses_duplicate_space_id() {
        let dir = temp_dir("dup-id");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        assert!(s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .is_err());
    }

    #[test]
    fn refuses_reserved_space_id() {
        let dir = temp_dir("reserved-id");
        let mut s = store(&dir);
        assert!(s
            .create_space(
                id("miyori-net"),
                id("spike"),
                digest(),
                label(),
                color(),
                false
            )
            .is_err());
    }

    #[test]
    fn config_is_written_atomically() {
        let dir = temp_dir("atomic-write");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        let space_dir = dir.join("spaces").join("a");
        let names: Vec<_> = std::fs::read_dir(&space_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["config.toml"],
            "временный файл не должен пережить создание спейса"
        );
    }

    #[test]
    fn set_clean_shutdown_persists_across_reload() {
        let dir = temp_dir("clean-shutdown-persists");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        s.set_clean_shutdown(&id("a"), false).unwrap();

        let reloaded = store(&dir);
        assert_eq!(reloaded.get(&id("a")).unwrap().clean_shutdown, Some(false));
    }

    #[test]
    fn set_clean_shutdown_errors_for_missing_space() {
        let dir = temp_dir("clean-shutdown-missing");
        let mut s = store(&dir);
        assert!(s.set_clean_shutdown(&id("ghost"), true).is_err());
    }

    #[test]
    fn set_digest_persists_across_reload() {
        let dir = temp_dir("digest-persists");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        let new_digest = "e".repeat(64);
        s.set_digest(&id("a"), new_digest.clone()).unwrap();

        let reloaded = store(&dir);
        assert_eq!(reloaded.get(&id("a")).unwrap().digest, new_digest);
    }

    #[test]
    fn set_digest_errors_for_missing_space() {
        let dir = temp_dir("digest-missing");
        let mut s = store(&dir);
        assert!(s.set_digest(&id("ghost"), digest()).is_err());
    }

    // config.toml, записанный до этой задачи, не содержит clean_shutdown вовсе — обязан читаться как None
    #[test]
    fn config_without_clean_shutdown_field_parses_as_none() {
        let toml = r##"
id = "a"
profile = "spike"
digest = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
cid = 3
uid = 70003
label = "untrusted"
color = "#e03131"
created = "2026-08-26T00:00:00Z"
"##;
        let config: SpaceConfig = basic_toml::from_str(toml).unwrap();
        assert_eq!(config.clean_shutdown, None);
    }

    // config.toml, записанный до задачи шифрования, не содержит encrypted вовсе — обязан читаться как false
    #[test]
    fn config_without_encrypted_field_parses_as_false() {
        let toml = r##"
id = "a"
profile = "spike"
digest = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
cid = 3
uid = 70003
label = "untrusted"
color = "#e03131"
created = "2026-08-26T00:00:00Z"
"##;
        let config: SpaceConfig = basic_toml::from_str(toml).unwrap();
        assert!(!config.encrypted);
    }

    #[test]
    fn create_space_persists_encrypted_flag() {
        let dir = temp_dir("encrypted-flag-persists");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), true)
            .unwrap();

        let reloaded = store(&dir);
        assert!(reloaded.get(&id("a")).unwrap().encrypted);
    }

    #[test]
    fn config_roundtrips() {
        let dir = temp_dir("roundtrip");
        let mut s = store(&dir);
        let cfg = s
            .create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap()
            .clone();
        let reloaded = store(&dir);
        assert_eq!(reloaded.get(&id("a")), Some(&cfg));
    }

    #[test]
    fn config_has_explicit_mode() {
        let dir = temp_dir("explicit-mode");
        let mut s = store(&dir);
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        let meta = std::fs::metadata(dir.join("spaces").join("a").join("config.toml")).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o644,
            "режим не должен зависеть от umask оператора"
        );
    }

    #[test]
    fn list_stays_sorted_after_create() {
        let dir = temp_dir("sorted-after-create");
        let mut s = store(&dir);
        s.create_space(id("b"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        s.create_space(id("a"), id("spike"), digest(), label(), color(), false)
            .unwrap();
        let created_order: Vec<&str> = s.list().iter().map(|c| c.id.as_ref()).collect();

        let reloaded = store(&dir);
        let loaded_order: Vec<&str> = reloaded.list().iter().map(|c| c.id.as_ref()).collect();

        assert_eq!(
            created_order, loaded_order,
            "порядок list() не должен зависеть от того, создан спейс в этом процессе или загружен с диска"
        );
    }

    #[test]
    fn rfc3339_from_epoch_matches_known_dates() {
        assert_eq!(rfc3339_from_epoch(0), "1970-01-01T00:00:00Z");
        // 2000-02-29 — високосный год, кратный 400 (единственное исключение из правила "не кратно 100")
        assert_eq!(rfc3339_from_epoch(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_epoch(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_epoch(1_767_225_599), "2025-12-31T23:59:59Z");
    }
}
