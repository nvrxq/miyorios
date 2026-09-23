#![forbid(unsafe_code)]

pub mod agent;
pub mod build;
pub mod net;
pub mod observe;
pub mod ops;
pub mod qemu;
pub mod render;
pub mod store;

use anyhow::{bail, Context, Result};
use std::path::Path;

// правило анти-спуфинга связывает tap, MAC и адрес одной строкой — разойдись формулы, кадры молча падали бы в drop
pub fn space_tap(cid: u32) -> String {
    format!("tap-space-{cid}")
}

pub fn space_mac(cid: u32) -> String {
    format!("52:54:00:6d:59:{cid:02x}")
}

pub fn space_ipv4(cid: u32) -> String {
    format!("10.59.0.{cid}")
}

// общая для main.rs (группа сокета) и qemu.rs (группа kvm) — gid не хардкодится, на другой машине он другой
pub fn lookup_gid(group_path: &Path, name: &str) -> Result<u32> {
    let src = std::fs::read_to_string(group_path)
        .with_context(|| format!("не читается {}", group_path.display()))?;
    for line in src.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            continue;
        }
        // формат /etc/group: name:password:gid:members — после name пропускаем password
        let gid = fields.nth(1).with_context(|| {
            format!(
                "некорректная строка группы {line:?} в {}",
                group_path.display()
            )
        })?;
        return gid
            .parse()
            .with_context(|| format!("gid {gid:?} группы {name:?} не является числом"));
    }
    bail!(
        "группа {name:?} не найдена в {}; заведите её перед установкой демона: groupadd --system {name}",
        group_path.display()
    );
}

// запись пользователя из /etc/passwd: gid — тот самый ПЕРВИЧНЫЙ gid, с которым сверяется newuidmap
#[derive(Debug)]
pub struct PasswdEntry {
    pub gid: u32,
    pub home: String,
}

// один проход по passwd — gid и home берутся из одной и той же найденной строки, второго прохода нет
pub fn lookup_passwd_entry(passwd_path: &Path, uid: u32) -> Result<PasswdEntry> {
    let src = std::fs::read_to_string(passwd_path)
        .with_context(|| format!("не читается {}", passwd_path.display()))?;
    let uid_str = uid.to_string();
    for line in src.lines() {
        let mut fields = line.split(':');
        // формат /etc/passwd: name:password:uid:gid:gecos:home:shell
        if fields.nth(2) != Some(uid_str.as_str()) {
            continue;
        }
        let gid = fields.next().with_context(|| {
            format!(
                "некорректная строка пользователя {line:?} в {}",
                passwd_path.display()
            )
        })?;
        let gid: u32 = gid
            .parse()
            .with_context(|| format!("gid {gid:?} пользователя uid={uid} не является числом"))?;
        // после gid остался gecos — пропускаем его, следующее поле и есть home
        let home = fields.nth(1).with_context(|| {
            format!(
                "некорректная строка пользователя {line:?} в {}",
                passwd_path.display()
            )
        })?;
        return Ok(PasswdEntry {
            gid,
            home: home.to_string(),
        });
    }
    bail!(
        "uid {uid} не найден в {}; сборщику неоткуда взять HOME и gid",
        passwd_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-lib-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // та же формула есть в miyori-init внутри образа, куда тестом не дотянуться — золотые значения держат их в согласии
    #[test]
    fn cid_derived_names_match_the_guest_formula() {
        assert_eq!(space_tap(3), "tap-space-3");
        assert_eq!(space_mac(3), "52:54:00:6d:59:03");
        assert_eq!(space_ipv4(3), "10.59.0.3");
        assert_eq!(space_mac(255), "52:54:00:6d:59:ff");
    }

    // uid, gid и gecos намеренно все разные числа/строки — перепутанные поля сразу дадут неверное значение
    #[test]
    fn lookup_passwd_entry_does_not_confuse_gid_with_home() {
        let dir = temp_dir("passwd-ok");
        let passwd = dir.join("passwd");
        std::fs::write(&passwd, "op:x:1000:2000:Op,,,:/home/op:/bin/bash\n").unwrap();
        let entry = lookup_passwd_entry(&passwd, 1000).unwrap();
        assert_eq!(entry.gid, 2000);
        assert_eq!(entry.home, "/home/op");
    }

    #[test]
    fn lookup_passwd_entry_errors_when_uid_is_absent() {
        let dir = temp_dir("passwd-missing-uid");
        let passwd = dir.join("passwd");
        std::fs::write(&passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        let err = lookup_passwd_entry(&passwd, 1000).unwrap_err();
        assert!(err.to_string().contains("1000"), "{err}");
    }

    #[test]
    fn lookup_passwd_entry_errors_on_malformed_line() {
        let dir = temp_dir("passwd-malformed-line");
        let passwd = dir.join("passwd");
        // строка обрывается сразу после uid — ни gid, ни home в ней нет
        std::fs::write(&passwd, "broken:x:1000\n").unwrap();
        let err = lookup_passwd_entry(&passwd, 1000).unwrap_err();
        assert!(err.to_string().contains("passwd"), "{err}");
    }
}
