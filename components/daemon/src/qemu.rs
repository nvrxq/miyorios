#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

// оснастка гейта M2 (spike-init ждёт эти переменные); настоящие спейсы не участвуют в её тестах
const DEFAULT_GUI_PORT: u32 = 1700;
const DEFAULT_FLOOD_SECONDS: u32 = 0;
const DEFAULT_NETROLE: &str = "none";

pub struct SpaceRun {
    pub id: String,
    pub cid: u32,
    pub uid: u32,
    pub template_dir: PathBuf,
    pub overlay: PathBuf,
    pub data: PathBuf,
    pub data_mb: u32,
    pub memory_mb: u32,
    pub cpus: u32,
    pub isolation_level: String,
    pub gpu: String,
    pub nonce: String,
    pub app_command: String,
    pub kvm_gid: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpaceState {
    Stopped,
    Running,
    // state_of сюда никогда не попадает: агента спрашивает crate::agent::observed_state поверх Running
    Unresponsive,
}

// как miyori_sandbox::build_command: аргументы — чистая функция, инварианты проверяются без KVM, tap и root
// secret_file — путь к файлу секрета для обоих томов; None у незашифрованного спейса (ADR-9)
pub fn build_argv(run: &SpaceRun, secret_file: Option<&Path>) -> Vec<String> {
    let tap = crate::space_tap(run.cid);
    let mac = crate::space_mac(run.cid);

    let mut a: Vec<String> = vec![
        // --no-new-privs обязателен: без него сброшенный uid вернул бы себе права через setuid-бинарь
        "setpriv".into(),
        "--reuid".into(),
        run.uid.to_string(),
        "--regid".into(),
        run.kvm_gid.to_string(),
        "--clear-groups".into(),
        "--no-new-privs".into(),
        "--".into(),
        "qemu-system-x86_64".into(),
        "-M".into(),
        "microvm,acpi=off,rtc=off".into(),
        "-enable-kvm".into(),
        "-cpu".into(),
        "host".into(),
        "-m".into(),
        run.memory_mb.to_string(),
        "-smp".into(),
        run.cpus.to_string(),
        "-nodefaults".into(),
        "-no-user-config".into(),
        "-nographic".into(),
        "-kernel".into(),
        run.template_dir.join("vmlinuz").display().to_string(),
        "-initrd".into(),
        run.template_dir.join("initrd.img").display().to_string(),
        "-append".into(),
        build_append(run),
    ];

    // -object secret обязан появиться раньше -drive, который на него ссылается
    let encrypt_suffix = match secret_file {
        Some(path) => {
            a.push("-object".into());
            a.push(format!("secret,id=sec0,file={},format=raw", path.display()));
            ",encrypt.key-secret=sec0"
        }
        None => "",
    };

    a.push("-drive".into());
    a.push(format!(
        "id=root,file={},format=qcow2,if=none,readonly=off{encrypt_suffix}",
        run.overlay.display()
    ));
    a.push("-device".into());
    a.push("virtio-blk-device,drive=root".into());

    // второй virtio-blk только если у профиля вообще заказан data-том
    if run.data_mb > 0 {
        a.push("-drive".into());
        a.push(format!(
            "id=data,file={},format=qcow2,if=none,readonly=off{encrypt_suffix}",
            run.data.display()
        ));
        a.push("-device".into());
        a.push("virtio-blk-device,drive=data".into());
    }

    a.push("-device".into());
    a.push(format!("vhost-vsock-device,guest-cid={}", run.cid));
    a.push("-netdev".into());
    a.push(format!("tap,id=net0,ifname={tap},script=no,downscript=no"));
    a.push("-device".into());
    a.push(format!("virtio-net-device,netdev=net0,mac={mac}"));
    a.push("-device".into());
    a.push("virtio-serial-device".into());
    a.push("-chardev".into());
    a.push("stdio,id=con".into());
    a.push("-device".into());
    a.push("virtconsole,chardev=con".into());

    // GPU включает именно gpu, а не level: reduced может быть заявлен и без графики
    if run.gpu == "virtio-gpu-venus" {
        a.push("-device".into());
        a.push("virtio-gpu-gl-device,hostmem=256M,blob=true,venus=true".into());
        a.push("-object".into());
        a.push(format!(
            "memory-backend-memfd,id=mem,size={}M,share=on",
            run.memory_mb
        ));
        a.push("-machine".into());
        a.push("memory-backend=mem".into());
    }

    a
}

fn build_append(run: &SpaceRun) -> String {
    [
        "console=hvc0".to_string(),
        "root=/dev/vda".to_string(),
        "rw".to_string(),
        "init=/usr/local/bin/miyori-init".to_string(),
        format!("MIYORI_PORT={DEFAULT_GUI_PORT}"),
        format!("MIYORI_CID={}", run.cid),
        format!("MIYORI_FLOOD_SECONDS={DEFAULT_FLOOD_SECONDS}"),
        format!("MIYORI_NETROLE={DEFAULT_NETROLE}"),
        "MIYORI_PEER_IP=".to_string(),
        "MIYORI_HOST_IPS=".to_string(),
        "MIYORI_ECHO_URL=".to_string(),
        format!("MIYORI_NONCE={}", run.nonce),
        // демон запускает приложение сам через агента (задача 7); автостарт в init выключен
        format!("MIYORI_APP={}", run.app_command),
        "MIYORI_APP_AUTOSTART=0".to_string(),
    ]
    .join(" ")
}

pub fn spawn(
    run: &SpaceRun,
    run_dir: &Path,
    state_dir: &Path,
    secret_file: Option<&Path>,
) -> Result<Child> {
    let argv = build_argv(run, secret_file);

    let log_dir = state_dir.join("spaces").join(&run.id);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("не удалось создать {}", log_dir.display()))?;
    let log_path = log_dir.join("last-run.log");
    let log_out = std::fs::File::create(&log_path)
        .with_context(|| format!("не удалось создать {}", log_path.display()))?;
    let log_err = log_out
        .try_clone()
        .with_context(|| format!("не удалось продублировать {}", log_path.display()))?;

    let pid_dir = run_dir.join(&run.id);
    std::fs::create_dir_all(&pid_dir)
        .with_context(|| format!("не удалось создать {}", pid_dir.display()))?;

    let child = Command::new(&argv[0])
        .args(&argv[1..])
        // у демона нет интерактивного терминала, который стоило бы отдавать гостевой консоли
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err)
        .spawn()
        .with_context(|| format!("не запускается QEMU для спейса \"{}\"", run.id))?;

    let pid_path = pid_dir.join("qemu.pid");
    std::fs::write(&pid_path, child.id().to_string())
        .with_context(|| format!("не удалось записать {}", pid_path.display()))?;

    Ok(child)
}

pub fn state_of(run_dir: &Path, id: &str, cid: u32) -> SpaceState {
    match pid_of(run_dir, id, cid) {
        Some(_) => SpaceState::Running,
        None => SpaceState::Stopped,
    }
}

pub fn pid_of(run_dir: &Path, id: &str, cid: u32) -> Option<u32> {
    let pid_path = run_dir.join(id).join("qemu.pid");
    let raw = std::fs::read_to_string(&pid_path).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    // PID переиспользуются: после перезапуска демона чужой процесс на этом PID не должен выглядеть спейсом
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if cmdline_identifies_space(&cmdline, cid) {
        Some(pid)
    } else {
        None
    }
}

fn cmdline_identifies_space(cmdline: &[u8], cid: u32) -> bool {
    contains_subslice(cmdline, b"qemu-system-x86_64")
        && contains_subslice(cmdline, format!("guest-cid={cid}").as_bytes())
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> SpaceRun {
        SpaceRun {
            id: "telegram".into(),
            cid: 5,
            uid: 70005,
            template_dir: PathBuf::from("/var/lib/miyorios/templates/telegram/deadbeef"),
            overlay: PathBuf::from("/var/lib/miyorios/spaces/telegram/system-overlay.qcow2"),
            data: PathBuf::from("/var/lib/miyorios/spaces/telegram/data.qcow2"),
            data_mb: 4096,
            memory_mb: 1024,
            cpus: 2,
            isolation_level: "standard".into(),
            gpu: "none".into(),
            nonce: "3f9a7c2e1b6d4508".into(),
            app_command: "/usr/bin/telegram-desktop".into(),
            kvm_gid: 108,
        }
    }

    fn flag_value<'a>(a: &'a [String], flag: &str) -> Vec<&'a str> {
        a.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn exactly_one_netdev_and_it_is_the_space_tap() {
        let a = build_argv(&run(), None);
        let netdevs = flag_value(&a, "-netdev");
        assert_eq!(netdevs.len(), 1, "{a:?}");
        assert!(netdevs[0].starts_with("tap,"), "{netdevs:?}");
        assert!(netdevs[0].contains("ifname=tap-space-5"), "{netdevs:?}");
    }

    #[test]
    fn never_user_mode_networking() {
        let a = build_argv(&run(), None);
        assert!(!a.iter().any(|s| s == "-net"), "{a:?}");
        assert!(!a.iter().any(|s| s == "-nic"), "{a:?}");
        assert!(
            !flag_value(&a, "-netdev")
                .iter()
                .any(|v| v.starts_with("user")),
            "{a:?}"
        );
    }

    #[test]
    fn defaults_and_user_config_are_off() {
        let a = build_argv(&run(), None);
        assert!(a.contains(&"-nodefaults".to_string()));
        assert!(a.contains(&"-no-user-config".to_string()));
    }

    #[test]
    fn guest_cid_matches_config() {
        let spec = run();
        let a = build_argv(&spec, None);
        let vsock = a
            .iter()
            .find(|s| s.contains("vhost-vsock-device"))
            .expect("нет vhost-vsock-device");
        assert!(vsock.contains(&format!("guest-cid={}", spec.cid)));
    }

    #[test]
    fn root_disk_is_the_overlay_not_the_template() {
        let spec = run();
        let a = build_argv(&spec, None);
        let root_drive = flag_value(&a, "-drive")
            .into_iter()
            .find(|d| d.starts_with("id=root,"))
            .expect("нет id=root");
        assert!(root_drive.contains(&format!("file={}", spec.overlay.display())));
        let template_root = spec.template_dir.join("root.qcow2").display().to_string();
        assert!(!root_drive.contains(&template_root));
    }

    #[test]
    fn template_is_only_a_backing_file() {
        let spec = run();
        let a = build_argv(&spec, None);
        let template_root = spec.template_dir.join("root.qcow2").display().to_string();
        assert!(
            !a.iter().any(|s| s.contains(&template_root)),
            "путь шаблонного диска не должен появляться в argv напрямую: {a:?}"
        );
    }

    #[test]
    fn standard_level_has_no_gpu_device() {
        let a = build_argv(&run(), None);
        assert!(!a.iter().any(|s| s.contains("virtio-gpu")), "{a:?}");
    }

    #[test]
    fn reduced_gpu_has_exactly_one_virtio_gpu_device() {
        let mut spec = run();
        spec.isolation_level = "reduced".into();
        spec.gpu = "virtio-gpu-venus".into();
        let a = build_argv(&spec, None);
        let gpu_devices = flag_value(&a, "-device")
            .into_iter()
            .filter(|d| d.contains("virtio-gpu"))
            .collect::<Vec<_>>();
        assert_eq!(gpu_devices.len(), 1, "{a:?}");
        assert_eq!(
            gpu_devices[0],
            "virtio-gpu-gl-device,hostmem=256M,blob=true,venus=true"
        );
    }

    #[test]
    fn reduced_level_without_gpu_still_has_no_gpu_device() {
        let mut spec = run();
        spec.isolation_level = "reduced".into();
        spec.gpu = "none".into();
        let a = build_argv(&spec, None);
        assert!(
            !a.iter().any(|s| s.contains("virtio-gpu")),
            "level сам по себе не должен включать GPU: {a:?}"
        );
    }

    #[test]
    fn reduced_gpu_adds_matching_memfd_backend() {
        let mut spec = run();
        spec.isolation_level = "reduced".into();
        spec.gpu = "virtio-gpu-venus".into();
        let a = build_argv(&spec, None);
        let objects = flag_value(&a, "-object");
        assert_eq!(objects.len(), 1, "{a:?}");
        assert_eq!(
            objects[0],
            format!(
                "memory-backend-memfd,id=mem,size={}M,share=on",
                spec.memory_mb
            )
        );
        let machines = flag_value(&a, "-machine");
        assert_eq!(machines, vec!["memory-backend=mem"], "{a:?}");
    }

    #[test]
    fn reduced_gpu_only_adds_graphics_args_nothing_else_changes() {
        let mut without_gpu = run();
        without_gpu.isolation_level = "reduced".into();
        without_gpu.gpu = "none".into();
        let base = build_argv(&without_gpu, None);

        let mut with_gpu = without_gpu;
        with_gpu.gpu = "virtio-gpu-venus".into();
        let with_gpu_argv = build_argv(&with_gpu, None);

        assert_eq!(
            with_gpu_argv[..base.len()],
            base[..],
            "vsock, tap и диски не должны меняться от появления gpu"
        );
        assert_eq!(
            &with_gpu_argv[base.len()..],
            &[
                "-device".to_string(),
                "virtio-gpu-gl-device,hostmem=256M,blob=true,venus=true".to_string(),
                "-object".to_string(),
                format!(
                    "memory-backend-memfd,id=mem,size={}M,share=on",
                    with_gpu.memory_mb
                ),
                "-machine".to_string(),
                "memory-backend=mem".to_string(),
            ],
            "{with_gpu_argv:?}"
        );
    }

    #[test]
    fn standard_level_has_no_vfio_device() {
        let a = build_argv(&run(), None);
        assert!(!a.iter().any(|s| s.contains("vfio")), "{a:?}");
    }

    #[test]
    fn memory_and_cpus_come_from_manifest() {
        let spec = run();
        let a = build_argv(&spec, None);
        assert_eq!(
            flag_value(&a, "-m"),
            vec![spec.memory_mb.to_string().as_str()]
        );
        assert_eq!(flag_value(&a, "-smp"), vec![spec.cpus.to_string().as_str()]);
    }

    #[test]
    fn append_carries_the_nonce() {
        let spec = run();
        let a = build_argv(&spec, None);
        let append = flag_value(&a, "-append")[0];
        assert!(append.contains(&format!("MIYORI_NONCE={}", spec.nonce)));
    }

    #[test]
    fn append_has_no_shell_metacharacters() {
        let a = build_argv(&run(), None);
        let append = flag_value(&a, "-append")[0];
        for ch in ['`', '$', ';', '|', '&', '>', '<', '\n', '"', '\'', '\\'] {
            assert!(!append.contains(ch), "метасимвол {ch:?} в append: {append}");
        }
    }

    #[test]
    fn data_disk_present_only_when_data_mb_positive() {
        let mut spec = run();
        spec.data_mb = 0;
        let a = build_argv(&spec, None);
        assert!(!a.iter().any(|s| s.contains("id=data")), "{a:?}");

        spec.data_mb = 4096;
        let a = build_argv(&spec, None);
        let data_drive = flag_value(&a, "-drive")
            .into_iter()
            .find(|d| d.starts_with("id=data,"))
            .expect("нет id=data при data_mb > 0");
        assert!(data_drive.contains(&format!("file={}", spec.data.display())));
    }

    #[test]
    fn no_secret_means_no_object_and_no_encrypt_key_secret() {
        let a = build_argv(&run(), None);
        assert!(!a.iter().any(|s| s.contains("secret,id=sec0")), "{a:?}");
        assert!(
            flag_value(&a, "-drive")
                .iter()
                .all(|d| !d.contains("encrypt")),
            "{a:?}"
        );
    }

    #[test]
    fn secret_object_precedes_drives_and_both_drives_get_key_secret() {
        let secret = PathBuf::from("/run/miyorios/secrets/telegram/pass");
        let a = build_argv(&run(), Some(&secret));

        let object_idx = a
            .iter()
            .position(|s| s == "-object")
            .expect("нет -object c секретом");
        assert_eq!(
            a[object_idx + 1],
            format!("secret,id=sec0,file={},format=raw", secret.display())
        );

        let first_drive_idx = a.iter().position(|s| s == "-drive").expect("нет -drive");
        assert!(
            object_idx < first_drive_idx,
            "-object обязан идти раньше -drive: {a:?}"
        );

        let drives = flag_value(&a, "-drive");
        assert_eq!(drives.len(), 2, "{a:?}");
        for d in drives {
            assert!(d.contains(",encrypt.key-secret=sec0"), "{d}");
        }
    }

    #[test]
    fn secret_without_data_volume_only_touches_the_one_drive_there_is() {
        let mut spec = run();
        spec.data_mb = 0;
        let secret = PathBuf::from("/run/miyorios/secrets/telegram/pass");
        let a = build_argv(&spec, Some(&secret));

        let drives = flag_value(&a, "-drive");
        assert_eq!(drives.len(), 1, "{a:?}");
        assert!(drives[0].contains(",encrypt.key-secret=sec0"));
    }

    #[test]
    fn privileges_are_dropped_before_qemu() {
        let spec = run();
        let a = build_argv(&spec, None);
        assert_eq!(a[0], "setpriv");
        assert_eq!(a[1], "--reuid");
        assert_eq!(a[2], spec.uid.to_string());
        assert_eq!(a[3], "--regid");
        assert_eq!(a[4], spec.kvm_gid.to_string());
        assert!(a.contains(&"--no-new-privs".to_string()));

        let sep = a
            .iter()
            .position(|s| s == "--")
            .expect("нет разделителя --");
        assert_eq!(a[sep + 1], "qemu-system-x86_64");
    }

    #[test]
    fn no_snapshot_flag() {
        let a = build_argv(&run(), None);
        assert!(!a.iter().any(|s| s == "-snapshot"), "{a:?}");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-qemu-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_pid_file_means_stopped() {
        let run_dir = temp_dir("no-pid-file");
        assert_eq!(state_of(&run_dir, "a", 3), SpaceState::Stopped);
    }

    #[test]
    fn pid_reuse_does_not_look_like_a_running_space() {
        let run_dir = temp_dir("pid-reuse");
        std::fs::create_dir_all(run_dir.join("a")).unwrap();

        // живой посторонний процесс на PID, который мог бы принадлежать спейсу до перезапуска демона
        let mut child = Command::new("sleep").arg("2").spawn().unwrap();
        std::fs::write(run_dir.join("a").join("qemu.pid"), child.id().to_string()).unwrap();

        let state = state_of(&run_dir, "a", 3);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(state, SpaceState::Stopped);
    }

    #[test]
    fn cmdline_matcher_recognizes_the_right_space() {
        let cmdline = b"qemu-system-x86_64\0-device\0vhost-vsock-device,guest-cid=3\0";
        assert!(cmdline_identifies_space(cmdline, 3));
    }

    #[test]
    fn cmdline_matcher_rejects_foreign_process() {
        let cmdline = b"sleep\x00300\0";
        assert!(!cmdline_identifies_space(cmdline, 3));
    }

    #[test]
    fn cmdline_matcher_rejects_wrong_cid() {
        let cmdline = b"qemu-system-x86_64\0-device\0vhost-vsock-device,guest-cid=4\0";
        assert!(!cmdline_identifies_space(cmdline, 3));
    }
}
