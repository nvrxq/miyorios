use crate::niri::Focus;
use crate::space::{self, CgroupSpace};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceInfo {
    pub id: String,
    pub color: String,
    pub level: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strip {
    Host,
    Space {
        id: String,
        color: String,
        level: String,
    },
    Orphan {
        id: String,
    },
    Unknown {
        id: String,
    },
    // заголовок называет спейс, а проверить нечем: подтверждать это словом "спейс" нельзя
    Unverified {
        claimed: String,
    },
    // фокус ещё не пришёл ни разу: заявлять "хост" на этом основании — та же ложь, что и дефект
    FocusUnknown,
}

fn resolve(space_id: &str, spaces: &[SpaceInfo]) -> Strip {
    match spaces.iter().find(|s| s.id == space_id) {
        Some(info) if info.state == "running" || info.state == "unresponsive" => Strip::Space {
            id: info.id.clone(),
            color: info.color.clone(),
            level: info.level.clone(),
        },
        Some(_stopped) => Strip::Orphan {
            id: space_id.to_string(),
        },
        None => Strip::Unknown {
            id: space_id.to_string(),
        },
    }
}

pub fn strip_for(focus: Focus<'_>, proc_dir: &Path, spaces: &[SpaceInfo]) -> Strip {
    let window = match focus {
        Focus::Unknown => return Strip::FocusUnknown,
        Focus::None => return Strip::Host,
        Focus::Window(window) => window,
    };

    // cgroup назначает systemd на хосте; заголовок — то место, куда дописывает гость
    let by_cgroup = match window.pid {
        Some(pid) => space::space_of_pid(proc_dir, pid),
        None => CgroupSpace::Unreadable,
    };
    match by_cgroup {
        CgroupSpace::Space(id) => return resolve(&id, spaces),
        // сильный признак сказал "не спейс" — заголовок после этого не спрашивают, иначе окно хоста переименует себя в спейс
        CgroupSpace::NotASpace => return Strip::Host,
        CgroupSpace::Unreadable => {}
    }

    match space::space_from_title(&window.title) {
        Some(claimed) => Strip::Unverified { claimed },
        None => Strip::Host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::niri::Window;
    use std::path::PathBuf;

    fn make_proc_dir(dir_name: &str, pid: u32, cgroup: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyori-label-strip-test-{}-{}",
            std::process::id(),
            dir_name
        ));
        let proc_pid_dir = dir.join(pid.to_string());
        std::fs::create_dir_all(&proc_pid_dir).unwrap();
        std::fs::write(proc_pid_dir.join("cgroup"), cgroup).unwrap();
        dir
    }

    fn empty_proc_dir(dir_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyori-label-strip-test-{}-{}",
            std::process::id(),
            dir_name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn running_spike() -> SpaceInfo {
        SpaceInfo {
            id: "spike".into(),
            color: "#ff0000".into(),
            level: "strict".into(),
            state: "running".into(),
        }
    }

    #[test]
    fn no_focus_is_host() {
        let dir = empty_proc_dir("no-focus");
        assert_eq!(strip_for(Focus::None, &dir, &[]), Strip::Host);
    }

    // незнание фокуса не может выглядеть как доверенный хост — это и есть исходный дефект
    #[test]
    fn unknown_focus_is_not_host() {
        let dir = empty_proc_dir("focus-unknown");
        assert_eq!(strip_for(Focus::Unknown, &dir, &[]), Strip::FocusUnknown);
    }

    #[test]
    fn window_no_pid_no_title_prefix_is_host() {
        let dir = empty_proc_dir("no-pid-no-title");
        let window = Window {
            id: 1,
            title: "обычное окно".into(),
            pid: None,
        };
        assert_eq!(strip_for(Focus::Window(&window), &dir, &[]), Strip::Host);
    }

    #[test]
    fn cgroup_names_running_space() {
        let dir = make_proc_dir(
            "running",
            100,
            "0::/user.slice/app.slice/miyori-gui-spike.scope",
        );
        let window = Window {
            id: 1,
            title: "weston-terminal".into(),
            pid: Some(100),
        };
        let spaces = vec![running_spike()];
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &spaces),
            Strip::Space {
                id: "spike".into(),
                color: "#ff0000".into(),
                level: "strict".into(),
            }
        );
    }

    #[test]
    fn cgroup_names_unresponsive_space_is_space() {
        let dir = make_proc_dir(
            "unresponsive",
            101,
            "0::/user.slice/app.slice/miyori-gui-spike.scope",
        );
        let window = Window {
            id: 1,
            title: "weston-terminal".into(),
            pid: Some(101),
        };
        let mut info = running_spike();
        info.state = "unresponsive".into();
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[info]),
            Strip::Space {
                id: "spike".into(),
                color: "#ff0000".into(),
                level: "strict".into(),
            }
        );
    }

    #[test]
    fn cgroup_names_stopped_space_is_orphan() {
        let dir = make_proc_dir(
            "stopped",
            102,
            "0::/user.slice/app.slice/miyori-gui-spike.scope",
        );
        let window = Window {
            id: 1,
            title: "weston-terminal".into(),
            pid: Some(102),
        };
        let mut info = running_spike();
        info.state = "stopped".into();
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[info]),
            Strip::Orphan { id: "spike".into() }
        );
    }

    #[test]
    fn cgroup_names_unknown_space() {
        let dir = make_proc_dir(
            "unknown",
            103,
            "0::/user.slice/app.slice/miyori-gui-ghost.scope",
        );
        let window = Window {
            id: 1,
            title: "weston-terminal".into(),
            pid: Some(103),
        };
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[running_spike()]),
            Strip::Unknown { id: "ghost".into() }
        );
    }

    // процесса уже нет, проверить заявку нечем: полоса говорит "заявлено", а не "спейс"
    #[test]
    fn pid_unreadable_leaves_the_title_claim_unverified() {
        let dir = empty_proc_dir("pid-gone");
        let window = Window {
            id: 1,
            title: "[space:spike] weston-terminal".into(),
            pid: Some(9999),
        };
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[running_spike()]),
            Strip::Unverified {
                claimed: "spike".into()
            }
        );
    }

    // окно хоста с подделанным заголовком: cgroup прочитан и говорит "не спейс", и это ответ окончательный
    #[test]
    fn host_window_with_forged_title_stays_host() {
        let dir = make_proc_dir(
            "host-forged-title",
            105,
            "0::/user.slice/app.slice/app-niri-wezterm-187653.scope",
        );
        let window = Window {
            id: 1,
            title: "[space:banking] Сбербанк".into(),
            pid: Some(105),
        };
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[running_spike()]),
            Strip::Host
        );
    }

    #[test]
    fn forged_title_does_not_override_real_cgroup() {
        let dir = make_proc_dir(
            "forged-title",
            104,
            "0::/user.slice/app.slice/miyori-gui-spike.scope",
        );
        let window = Window {
            id: 1,
            title: "[space:banking] Сбербанк".into(),
            pid: Some(104),
        };
        assert_eq!(
            strip_for(Focus::Window(&window), &dir, &[running_spike()]),
            Strip::Space {
                id: "spike".into(),
                color: "#ff0000".into(),
                level: "strict".into(),
            }
        );
    }
}
