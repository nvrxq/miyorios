#![forbid(unsafe_code)]

use anyhow::{bail, Context, Result};
use miyori_proto::ids::{Color, Label, SpaceId};
use serde::{Deserialize, Serialize};
use std::path::Path;

// зарезервировано планом: create отвергает этот id, а реестр всегда содержит его сам (решение D)
pub const MIYORI_NET_ID: &str = "miyori-net";
const MIYORI_NET_CID: u32 = 9;
const MIYORI_NET_LABEL: &str = "trusted";
const MIYORI_NET_COLOR: &str = "#2f9e44";
const MAX_CID: u32 = 255;

pub struct RegistryEntry {
    pub id: SpaceId,
    pub cid: u32,
    pub label: Label,
    pub color: Color,
}

fn miyori_net_entry() -> RegistryEntry {
    RegistryEntry {
        id: SpaceId::new(MIYORI_NET_ID).expect("MIYORI_NET_ID — заведомо безопасный слаг"),
        cid: MIYORI_NET_CID,
        label: Label::new(MIYORI_NET_LABEL).expect("MIYORI_NET_LABEL — заведомо безопасный слаг"),
        color: Color::new(MIYORI_NET_COLOR).expect("MIYORI_NET_COLOR — заведомо формата #RRGGBB"),
    }
}

#[derive(Deserialize)]
struct RawRegistry {
    #[serde(default, rename = "space")]
    space: Vec<RawSpace>,
}

#[derive(Deserialize)]
struct RawSpace {
    id: String,
    cid: u32,
    label: String,
    color: String,
}

#[derive(Serialize)]
struct TomlRegistry<'a> {
    space: Vec<TomlSpace<'a>>,
}

#[derive(Serialize)]
struct TomlSpace<'a> {
    id: &'a str,
    cid: u32,
    label: &'a str,
    color: &'a str,
}

// читает готовый spaces.toml (так его зовёт net-fixture.sh), а не состояние демона
pub fn load_registry_file(path: &Path) -> Result<Vec<RegistryEntry>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("не читается реестр {}", path.display()))?;
    let raw: RawRegistry = basic_toml::from_str(&src).context("некорректный TOML реестра")?;
    raw.space
        .into_iter()
        .map(|s| {
            Ok(RegistryEntry {
                id: SpaceId::new(&s.id)?,
                cid: s.cid,
                label: Label::new(&s.label)?,
                color: Color::new(&s.color)?,
            })
        })
        .collect()
}

// вход — минимальный список (id, cid, label, color): и state-dir демона, и файл фикстуры сводятся к нему
pub fn render_registry(entries: &[RegistryEntry]) -> String {
    let net = miyori_net_entry();
    let mut rows: Vec<&RegistryEntry> = entries
        .iter()
        .filter(|e| e.id.as_ref() != MIYORI_NET_ID)
        .collect();
    rows.push(&net);
    rows.sort_by_key(|e| e.cid);

    let toml = TomlRegistry {
        space: rows
            .iter()
            .map(|e| TomlSpace {
                id: e.id.as_ref(),
                cid: e.cid,
                label: e.label.as_ref(),
                color: e.color.as_ref(),
            })
            .collect(),
    };
    basic_toml::to_string(&toml).expect(
        "TomlRegistry/TomlSpace — derive(Serialize) над &str/u32: serialize_struct, а не serialize_map \
         (KeyNotString недостижим), нет полей-enum/tuple/Option (UnsupportedType/UnsupportedNone недостижимы), \
         нет табличного поля перед скалярным — у TomlSpace таких полей нет вовсе, у TomlRegistry оно одно \
         (ValueAfterTable недостижим), а Display у &str/u32 не отказывает (Custom недостижим)",
    )
}

pub fn render_nft(entries: &[RegistryEntry]) -> Result<String> {
    let mut spaces: Vec<&RegistryEntry> = entries
        .iter()
        .filter(|e| e.id.as_ref() != MIYORI_NET_ID)
        .collect();
    for space in &spaces {
        // 10.59.0.$cid и 52:54:00:6d:59:%02x перестают быть однозначными за 255
        if space.cid > MAX_CID {
            bail!(
                "cid {} превышает {MAX_CID}: адрес и MAC спейса строятся из одного октета",
                space.cid
            );
        }
    }
    spaces.sort_by_key(|e| e.cid);

    let mut out = String::new();
    out.push_str("#!/usr/sbin/nft -f\n");
    out.push_str(
        "# анти-спуфинг вместо DHCP-lease в miyori-net: демон lease — лишний TCB в роутере,\n",
    );
    out.push_str(
        "# а лгущего гостя он всё равно не остановит — останавливает только эта фильтрация\n",
    );
    out.push_str("add table bridge miyori\n");
    out.push_str("flush table bridge miyori\n");
    out.push('\n');
    out.push_str("table bridge miyori {\n");
    out.push_str("\tchain spaces {\n");
    out.push_str("\t\ttype filter hook forward priority filter; policy accept;\n");
    out.push('\n');
    out.push_str("\t\t# не трогаем чужие мосты хоста: docker, libvirt, br-captive\n");
    out.push_str("\t\tmeta ibrname != \"br-spaces\" accept\n");
    out.push('\n');
    out.push_str("\t\tiifname \"tap-spaces\" accept\n");
    out.push('\n');
    for space in &spaces {
        let cid = space.cid;
        let tap = crate::space_tap(cid);
        let mac = crate::space_mac(cid);
        let ip = crate::space_ipv4(cid);
        out.push_str(&format!(
            "\t\tiifname \"{tap}\" ether saddr {mac} ip saddr {ip} accept\n"
        ));
        out.push_str(&format!(
            "\t\tiifname \"{tap}\" ether saddr {mac} arp saddr ip {ip} accept\n"
        ));
    }
    out.push('\n');
    out.push_str("\t\t# спуф MAC/IP/ARP и весь IPv6 с портов спейсов падают сюда\n");
    out.push_str("\t\tdrop\n");
    out.push_str("\t}\n");
    out.push('\n');
    out.push_str(
        "\t# находка теста 52: хост отвечал спейсу по link-local IPv6. Кадры, адресованные\n",
    );
    out.push_str("\t# самому мосту, идут через input и под цепочку forward не попадают вовсе.\n");
    out.push_str("\t# Хост не участник этого моста — ему отсюда не адресуют ничего\n");
    out.push_str("\tchain to-host {\n");
    out.push_str("\t\ttype filter hook input priority filter; policy accept;\n");
    out.push_str("\t\tmeta ibrname != \"br-spaces\" accept\n");
    out.push_str("\t\tcounter drop\n");
    out.push_str("\t}\n");
    out.push_str("}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, cid: u32) -> RegistryEntry {
        RegistryEntry {
            id: SpaceId::new(id).unwrap(),
            cid,
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
        }
    }

    fn temp_registry_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "miyorid-render-test-{}-{n}.toml",
            std::process::id()
        ))
    }

    fn roundtrip(entries: &[RegistryEntry]) -> Vec<RegistryEntry> {
        let toml = render_registry(entries);
        let path = temp_registry_path();
        std::fs::write(&path, &toml).unwrap();
        let loaded = load_registry_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        loaded
    }

    #[test]
    fn registry_always_contains_miyori_net() {
        let loaded = roundtrip(&[]);
        let net = loaded
            .iter()
            .find(|e| e.id.as_ref() == "miyori-net")
            .expect("реестр обязан содержать miyori-net");
        assert_eq!(net.cid, 9);
        assert_eq!(net.label.as_ref(), "trusted");
    }

    #[test]
    fn registry_matches_config_of_each_space() {
        let entries = vec![entry("spike", 3), entry("spike2", 4)];
        let loaded = roundtrip(&entries);
        for e in &entries {
            let found = loaded
                .iter()
                .find(|l| l.id == e.id)
                .unwrap_or_else(|| panic!("{} не найден в отрендеренном реестре", e.id));
            assert_eq!(found.cid, e.cid);
            assert_eq!(found.label, e.label);
            assert_eq!(found.color, e.color);
        }
    }

    #[test]
    fn nft_has_two_rules_per_space() {
        let entries = vec![entry("spike", 3), entry("spike2", 4)];
        let nft = render_nft(&entries).unwrap();
        assert_eq!(nft.matches("tap-space-3\"").count(), 2);
        assert_eq!(nft.matches("tap-space-4\"").count(), 2);
    }

    #[test]
    fn nft_rule_binds_tap_mac_and_ip_together() {
        let nft = render_nft(&[entry("spike", 3)]).unwrap();

        let ip_line = nft
            .lines()
            .find(|l| l.contains("ip saddr 10.59.0.3"))
            .expect("нет строки ip saddr для cid 3");
        assert!(ip_line.contains("tap-space-3"));
        assert!(ip_line.contains("52:54:00:6d:59:03"));

        let arp_line = nft
            .lines()
            .find(|l| l.contains("arp saddr ip 10.59.0.3"))
            .expect("нет строки arp saddr ip для cid 3");
        assert!(arp_line.contains("tap-space-3"));
        assert!(arp_line.contains("52:54:00:6d:59:03"));
    }

    #[test]
    fn nft_ends_with_drop_not_accept() {
        let nft = render_nft(&[entry("spike", 3)]).unwrap();
        assert!(
            nft.contains("\t\tdrop\n\t}\n"),
            "цепочка spaces обязана заканчиваться drop, а не accept по умолчанию"
        );
    }

    #[test]
    fn nft_keeps_to_host_chain() {
        let nft = render_nft(&[]).unwrap();
        assert!(nft.contains("chain to-host"));
        assert!(nft.contains("counter drop"));
    }

    #[test]
    fn cid_above_255_is_refused() {
        let entries = vec![entry("spike", 256)];
        assert!(render_nft(&entries).is_err());
    }
}
