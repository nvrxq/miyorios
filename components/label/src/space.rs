use std::path::Path;

pub fn space_from_cgroup(cgroup: &str) -> Option<String> {
    for line in cgroup.lines() {
        // не bail-аутить на первой несовпавшей строке: cgroup v1 может нести несколько
        let Some(scope) = line.rsplit('/').next() else {
            continue;
        };
        let id = scope
            .strip_prefix("miyori-gui-")
            .and_then(|s| s.strip_suffix(".scope"));
        if let Some(id) = id {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

pub fn space_from_title(title: &str) -> Option<String> {
    let rest = title.strip_prefix("[space:")?;
    let (id, _) = rest.split_once("] ")?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

// «не спейс» и «не смогли прочитать» — разные ответы: на первом заголовок уже спрашивать нельзя
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupSpace {
    Space(String),
    NotASpace,
    Unreadable,
}

pub fn space_of_pid(proc_dir: &Path, pid: u32) -> CgroupSpace {
    let path = proc_dir.join(pid.to_string()).join("cgroup");
    let Ok(cgroup) = std::fs::read_to_string(path) else {
        return CgroupSpace::Unreadable;
    };
    match space_from_cgroup(&cgroup) {
        Some(id) => CgroupSpace::Space(id),
        None => CgroupSpace::NotASpace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_real_line() {
        let cgroup =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/miyori-gui-spike.scope";
        assert_eq!(space_from_cgroup(cgroup), Some("spike".to_string()));
    }

    #[test]
    fn cgroup_host_window() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-niri-wezterm-187653.scope";
        assert_eq!(space_from_cgroup(cgroup), None);
    }

    #[test]
    fn cgroup_id_with_dash() {
        let cgroup = "0::/user.slice/user-1000.slice/app.slice/miyori-gui-my-space.scope";
        assert_eq!(space_from_cgroup(cgroup), Some("my-space".to_string()));
    }

    #[test]
    fn cgroup_empty_id() {
        let cgroup = "0::/user.slice/app.slice/miyori-gui-.scope";
        assert_eq!(space_from_cgroup(cgroup), None);
    }

    #[test]
    fn cgroup_substring_not_scope_name() {
        let cgroup = "0::/user.slice/miyori-gui-fake/app.slice/some-other.scope";
        assert_eq!(space_from_cgroup(cgroup), None);
    }

    #[test]
    fn title_simple_prefix() {
        assert_eq!(
            space_from_title("[space:spike] weston-terminal"),
            Some("spike".to_string())
        );
    }

    #[test]
    fn title_guest_forged_second_prefix() {
        assert_eq!(
            space_from_title("[space:spike] [space:banking] Сбербанк"),
            Some("spike".to_string())
        );
    }

    #[test]
    fn title_no_prefix() {
        assert_eq!(space_from_title("обычное окно"), None);
    }

    #[test]
    fn title_prefix_not_at_start() {
        assert_eq!(space_from_title(" [space:spike] x"), None);
    }

    #[test]
    fn title_empty_id() {
        assert_eq!(space_from_title("[space:] x"), None);
    }

    #[test]
    fn pid_reads_cgroup_from_given_dir() {
        let dir = std::env::temp_dir().join(format!(
            "miyori-label-test-{}-{}",
            std::process::id(),
            "pid-reads-cgroup"
        ));
        let proc_pid_dir = dir.join("4242");
        std::fs::create_dir_all(&proc_pid_dir).unwrap();
        std::fs::write(
            proc_pid_dir.join("cgroup"),
            "0::/user.slice/app.slice/miyori-gui-spike.scope\n",
        )
        .unwrap();

        assert_eq!(
            space_of_pid(&dir, 4242),
            CgroupSpace::Space("spike".to_string())
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pid_missing_process_is_unreadable_not_not_a_space() {
        let dir = std::env::temp_dir().join(format!(
            "miyori-label-test-{}-{}",
            std::process::id(),
            "pid-missing"
        ));
        assert_eq!(space_of_pid(&dir, 9999), CgroupSpace::Unreadable);
    }

    // окно хоста: cgroup прочитан и говорит "не спейс" — это ответ, а не отсутствие ответа
    #[test]
    fn host_process_cgroup_is_not_a_space() {
        let dir = std::env::temp_dir().join(format!(
            "miyori-label-test-{}-{}",
            std::process::id(),
            "pid-host"
        ));
        let proc_pid_dir = dir.join("4243");
        std::fs::create_dir_all(&proc_pid_dir).unwrap();
        std::fs::write(
            proc_pid_dir.join("cgroup"),
            "0::/user.slice/app.slice/app-niri-wezterm-187653.scope\n",
        )
        .unwrap();

        assert_eq!(space_of_pid(&dir, 4243), CgroupSpace::NotASpace);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
