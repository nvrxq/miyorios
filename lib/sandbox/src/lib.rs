#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command};

pub struct SandboxSpec {
    pub space_id: String,
    pub socket: PathBuf,
    pub secctx: String,
    pub title_prefix: String,
    pub waypipe: PathBuf,
    pub wayland_socket: PathBuf,
    pub memory_max_mb: u64,
    pub tasks_max: u64,
    pub cpu_quota_pct: u64,
}

// приватный runtime внутри песочницы; наружу отсюда виден только сокет композитора
const SANDBOX_RUNTIME: &str = "/run/wp";
const SANDBOX_WAYLAND: &str = "wayland-0";

pub fn build_command(spec: &SandboxSpec) -> Vec<String> {
    let socket_dir = spec
        .socket
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/run/miyorios"));
    let mut a: Vec<String> = vec![
        "systemd-run".into(),
        "--user".into(),
        "--scope".into(),
        "--collect".into(),
        format!("--unit=miyori-gui-{}", spec.space_id),
        format!("--property=MemoryMax={}M", spec.memory_max_mb),
        format!("--property=TasksMax={}", spec.tasks_max),
        format!("--property=CPUQuota={}%", spec.cpu_quota_pct),
        "--".into(),
        "bwrap".into(),
        "--unshare-all".into(),
        "--unshare-net".into(),
        "--die-with-parent".into(),
        "--new-session".into(),
        "--clearenv".into(),
    ];

    for dir in [
        "/usr",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc/ld.so.cache",
    ] {
        a.push("--ro-bind-try".into());
        a.push(dir.into());
        a.push(dir.into());
    }

    a.push("--proc".into());
    a.push("/proc".into());
    a.push("--dev".into());
    a.push("/dev".into());
    a.push("--tmpfs".into());
    a.push("/tmp".into());

    // XDG_RUNTIME_DIR хоста содержит niri IPC, сессионный D-Bus и pipewire — внутрь идёт пустой tmpfs
    a.push("--tmpfs".into());
    a.push(SANDBOX_RUNTIME.into());

    a.push("--ro-bind".into());
    a.push(spec.wayland_socket.display().to_string());
    a.push(format!("{SANDBOX_RUNTIME}/{SANDBOX_WAYLAND}"));

    // каталог у каждого спейса свой, иначе прокси видел бы сокеты соседей
    a.push("--bind".into());
    a.push(socket_dir.display().to_string());
    a.push(socket_dir.display().to_string());

    a.push("--setenv".into());
    a.push("XDG_RUNTIME_DIR".into());
    a.push(SANDBOX_RUNTIME.into());
    a.push("--setenv".into());
    a.push("WAYLAND_DISPLAY".into());
    a.push(SANDBOX_WAYLAND.into());

    a.push("--".into());
    a.push(spec.waypipe.display().to_string());
    a.push("--no-gpu".into());
    // без явного флага waypipe стартует с дефолтным lz4 и молча падает в none, см. ADR-4
    a.push("-c".into());
    a.push("none".into());
    a.push("--secctx".into());
    a.push(spec.secctx.clone());
    a.push("--title-prefix".into());
    a.push(spec.title_prefix.clone());
    a.push("--socket".into());
    a.push(spec.socket.display().to_string());
    a.push("client".into());
    a
}

pub fn spawn(spec: &SandboxSpec) -> Result<Child> {
    let argv = build_command(spec);
    Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .with_context(|| format!("не запускается GUI-песочница для {}", spec.space_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            space_id: "spike".into(),
            socket: PathBuf::from("/run/miyorios/spike/gui/gui.sock"),
            secctx: "org.miyorios.space.spike".into(),
            title_prefix: "[space:spike] ".into(),
            waypipe: PathBuf::from("/usr/local/lib/miyorios/waypipe"),
            wayland_socket: PathBuf::from("/run/user/1000/wayland-1"),
            memory_max_mb: 512,
            tasks_max: 64,
            cpu_quota_pct: 50,
        }
    }

    fn argv() -> Vec<String> {
        build_command(&spec())
    }

    #[test]
    fn applies_resource_limits() {
        let a = argv();
        assert!(a.contains(&"--property=MemoryMax=512M".to_string()));
        assert!(a.contains(&"--property=TasksMax=64".to_string()));
        assert!(a.contains(&"--property=CPUQuota=50%".to_string()));
    }

    #[test]
    fn denies_network_and_home() {
        let a = argv();
        assert!(a.contains(&"--unshare-net".to_string()));
        // никакой bind $HOME внутрь песочницы
        assert!(!a.iter().any(|s| s.contains("/home/")));
    }

    #[test]
    fn binds_only_the_compositor_socket() {
        let a = argv();
        let binds: Vec<_> = a
            .windows(3)
            .filter(|w| w[0] == "--ro-bind" || w[0] == "--bind")
            .map(|w| (w[1].clone(), w[2].clone()))
            .collect();

        // белый список, а не перечисление запрещённых имён: забытое имя — это дыра
        for (src, _) in &binds {
            assert!(
                src == "/run/user/1000/wayland-1" || src.starts_with("/run/miyorios/spike"),
                "посторонний bind: {src}"
            );
        }
        assert!(binds.iter().any(|(_, dst)| dst == "/run/wp/wayland-0"));
    }

    #[test]
    fn host_runtime_dir_is_never_exposed() {
        let a = argv();
        // в каталоге хоста лежат niri IPC, сессионный D-Bus и сокеты pipewire
        assert!(
            !a.iter().any(|s| s == "/run/user/1000"),
            "XDG_RUNTIME_DIR хоста попал в argv: {a:?}"
        );
        let tmpfs: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "--tmpfs")
            .map(|w| w[1].clone())
            .collect();
        assert!(tmpfs.contains(&"/run/wp".to_string()));
    }

    #[test]
    fn overrides_runtime_environment() {
        let a = argv();
        let value = |k: &str| {
            a.windows(3)
                .find(|w| w[0] == "--setenv" && w[1] == k)
                .map(|w| w[2].clone())
        };
        assert_eq!(value("XDG_RUNTIME_DIR").as_deref(), Some("/run/wp"));
        assert_eq!(value("WAYLAND_DISPLAY").as_deref(), Some("wayland-0"));
    }

    #[test]
    fn passes_host_controlled_identity_to_waypipe() {
        let a = argv();
        assert!(a.contains(&"--secctx".to_string()));
        assert!(a.contains(&"org.miyorios.space.spike".to_string()));
        assert!(a.contains(&"--title-prefix".to_string()));
        assert!(a.contains(&"[space:spike] ".to_string()));
        assert!(a.contains(&"--no-gpu".to_string()));
        assert!(a.contains(&"client".to_string()));
    }

    #[test]
    fn compression_is_disabled_explicitly() {
        let a = argv();
        let value = a.windows(2).find(|w| w[0] == "-c").map(|w| w[1].clone());
        assert_eq!(value.as_deref(), Some("none"));
    }

    #[test]
    fn never_enables_vsock_on_host_side() {
        // хост-сторона слушает unix-сокет; vsock принадлежит брокеру
        assert!(!argv().contains(&"--vsock".to_string()));
    }
}
