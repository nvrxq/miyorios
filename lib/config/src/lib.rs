#![forbid(unsafe_code)]

// подкаталог спейса в /run/miyorios, открытый группе сокета: только туда пишет
// непривилегированный miyori-guid. Сам каталог спейса держит состояние демона
pub const GUI_SUBDIR: &str = "gui";

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

// CID 0-2 зарезервированы ядром (hypervisor/local/host), спейсу их отдавать нельзя
const RESERVED_CIDS: [u32; 3] = [0, 1, 2];

#[derive(Debug, Deserialize)]
pub struct Space {
    pub id: String,
    pub cid: u32,
    pub label: String,
    pub color: String,
}

impl Space {
    pub fn title_prefix(&self) -> String {
        format!("[space:{}] ", self.id)
    }

    pub fn secctx_id(&self) -> String {
        format!("org.miyorios.space.{}", self.id)
    }
}

#[derive(Debug, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "space")]
    spaces: Vec<Space>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("не читается реестр {}", path.display()))?;
        Self::parse(&src)
    }

    pub fn parse(src: &str) -> Result<Self> {
        let registry: Registry = basic_toml::from_str(src).context("некорректный TOML реестра")?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn by_cid(&self, cid: u32) -> Option<&Space> {
        self.spaces.iter().find(|s| s.cid == cid)
    }

    fn validate(&self) -> Result<()> {
        let mut seen_cid = HashSet::new();
        let mut seen_id = HashSet::new();
        for space in &self.spaces {
            if !is_safe_slug(&space.id) {
                bail!("id {:?} не является безопасным слагом [a-z0-9-]", space.id);
            }
            if RESERVED_CIDS.contains(&space.cid) {
                bail!("cid {} зарезервирован ядром", space.cid);
            }
            if !seen_cid.insert(space.cid) {
                bail!("cid {} назначен более чем одному спейсу", space.cid);
            }
            if !seen_id.insert(space.id.clone()) {
                bail!("id {:?} встречается дважды", space.id);
            }
        }
        Ok(())
    }
}

pub fn is_safe_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    // r##"…"## обязателен: внутри есть "#RRGGBB, а "# закрыло бы обычную raw-строку
    fn registry() -> Registry {
        Registry::parse(
            r##"
            [[space]]
            id = "telegram"
            cid = 3
            label = "personal"
            color = "#2f9e44"

            [[space]]
            id = "ctf"
            cid = 4
            label = "untrusted"
            color = "#e03131"
            "##,
        )
        .unwrap()
    }

    #[test]
    fn resolves_space_by_cid() {
        assert_eq!(registry().by_cid(4).unwrap().id, "ctf");
    }

    #[test]
    fn unknown_cid_resolves_to_nothing() {
        assert!(registry().by_cid(99).is_none());
    }

    #[test]
    fn duplicate_cid_is_rejected() {
        let err = Registry::parse(
            r##"
            [[space]]
            id = "a"
            cid = 3
            label = "x"
            color = "#000000"

            [[space]]
            id = "b"
            cid = 3
            label = "y"
            color = "#000000"
            "##,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cid 3"));
    }

    #[test]
    fn reserved_cids_are_rejected() {
        for cid in [0, 1, 2] {
            let src =
                format!("[[space]]\nid = \"a\"\ncid = {cid}\nlabel = \"x\"\ncolor = \"#000000\"\n");
            assert!(
                Registry::parse(&src).is_err(),
                "cid {cid} должен быть отвергнут"
            );
        }
    }

    #[test]
    fn identifiers_are_derived_from_id_not_from_guest() {
        let r = registry();
        let s = r.by_cid(3).unwrap();
        assert_eq!(s.title_prefix(), "[space:telegram] ");
        assert_eq!(s.secctx_id(), "org.miyorios.space.telegram");
    }

    #[test]
    fn id_must_be_safe_slug() {
        let src = "[[space]]\nid = \"bad id/../x\"\ncid = 3\nlabel = \"x\"\ncolor = \"#000000\"\n";
        assert!(Registry::parse(src).is_err());
    }
}
