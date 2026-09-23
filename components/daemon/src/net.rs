#![forbid(unsafe_code)]

// tap спейса и bridge-правила; мостами, VFIO и запуском miyori-net демон не владеет (решение C)

use crate::render::{self, RegistryEntry};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const BRIDGE: &str = "br-spaces";
pub const CAPTIVE_BRIDGE: &str = "br-captive";
// тот же адрес и та же переменная, что уже читают run-miyori-net.sh/vfio-bind.sh/host-online.sh
const DEFAULT_UPLINK_PCI: &str = "0000:0c:00.0";
// консоль miyori-net растёт, пока она жива; больше этого объёма в память не тянем (решение H)
const CONSOLE_TAIL_BYTES: u64 = 64 * 1024;
// ruleset может быть большим, а обрезка обязана быть видимой: усечённый фаервол, выданный за полный, хуже отсутствующего
pub const RULESET_MAX_CHARS: usize = 120 * 1024;

pub fn bridge_exists() -> bool {
    link_exists(BRIDGE)
}

pub fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// изоляция ставится сразу после подключения к мосту, до "up" — окно между ними спейс видел бы соседей (тест 51)
pub fn create_tap(cid: u32, uid: u32) -> Result<()> {
    let tap = crate::space_tap(cid);
    let uid = uid.to_string();
    run(
        "ip",
        &["tuntap", "add", "dev", &tap, "mode", "tap", "user", &uid],
    )?;
    run("ip", &["link", "set", &tap, "master", BRIDGE])?;
    run("bridge", &["link", "set", "dev", &tap, "isolated", "on"])?;
    run("ip", &["link", "set", &tap, "up"])?;
    Ok(())
}

pub fn delete_tap(cid: u32) -> Result<()> {
    let tap = crate::space_tap(cid);
    run("ip", &["link", "del", &tap])
}

// внешний kill -9 демона/QEMU оставляет tap висеть — TUNSETIFF на то же имя падает "Device or resource busy" навсегда
pub fn reap_leftover_tap(cid: u32) -> Result<()> {
    let tap = crate::space_tap(cid);
    if link_exists(&tap) {
        delete_tap(cid)?;
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("не удалось запустить {program} {args:?}"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} завершился с {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// вызывается после create/destroy: состав спейсов изменился, реестр и bridge-правила должны это отразить (решение D)
pub fn refresh(store: &Store, registry_path: &Path) -> Result<()> {
    let entries: Vec<RegistryEntry> = store
        .list()
        .iter()
        .map(|c| RegistryEntry {
            id: c.id.clone(),
            cid: c.cid,
            label: c.label.clone(),
            color: c.color.clone(),
        })
        .collect();

    write_atomically(registry_path, &render::render_registry(&entries))?;
    apply_nft(&render::render_nft(&entries)?)?;
    Ok(())
}

fn write_atomically(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "у пути реестра {} нет родительского каталога",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("не удалось создать {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("путь реестра без имени файла")?;
    let tmp_path = parent.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("не удалось записать {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "не удалось переименовать {} в {}",
            tmp_path.display(),
            path.display()
        )
    })
}

// nft -f - применяет flush table и новые правила одной транзакцией: окна с пустой таблицей не возникает
fn apply_nft(rules: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("не удалось запустить nft")?;
    child
        .stdin
        .take()
        .context("у процесса nft нет stdin")?
        .write_all(rules.as_bytes())
        .context("не удалось передать правила в nft")?;
    let status = child.wait().context("nft не завершился")?;
    if !status.success() {
        bail!("nft -f - завершился с {status}");
    }
    Ok(())
}

// net-status (задача 11) только наблюдает — мосты, tap'ы, vfio-pci и miyori-net демону не принадлежат (решение C)

pub struct TapStatus {
    pub name: String,
    pub isolated: Option<bool>,
}

// тест подставляет свой каталог вместо /sys/class/net — так же, как Store делает для net_dir
pub fn list_space_taps() -> Vec<TapStatus> {
    list_space_taps_at(Path::new("/sys/class/net"))
}

fn list_space_taps_at(net_class_dir: &Path) -> Vec<TapStatus> {
    let mut taps = Vec::new();
    let entries = match std::fs::read_dir(net_class_dir) {
        Ok(entries) => entries,
        Err(_) => return taps,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("tap-space-") {
            continue;
        }
        let isolated = std::fs::read_to_string(entry.path().join("brport").join("isolated"))
            .ok()
            .and_then(|raw| parse_isolated_flag(&raw));
        taps.push(TapStatus { name, isolated });
    }
    taps.sort_by(|a, b| a.name.cmp(&b.name));
    taps
}

// sysfs всегда пишет "0\n"/"1\n"; что-то ещё — испорченный или неожиданный формат, а не факт про изоляцию
fn parse_isolated_flag(raw: &str) -> Option<bool> {
    match raw.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

pub struct PciStatus {
    pub address: String,
    pub present: bool,
    pub driver: Option<String>,
}

pub fn uplink_pci_addr() -> String {
    std::env::var("MIYORI_UPLINK_PCI").unwrap_or_else(|_| DEFAULT_UPLINK_PCI.to_string())
}

pub fn pci_status(addr: &str) -> PciStatus {
    pci_status_at(Path::new("/sys/bus/pci/devices"), addr)
}

fn pci_status_at(devices_dir: &Path, addr: &str) -> PciStatus {
    let dev_dir = devices_dir.join(addr);
    let driver = std::fs::read_link(dev_dir.join("driver"))
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
    PciStatus {
        address: addr.to_string(),
        present: dev_dir.exists(),
        driver,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TunnelBlock {
    pub ifc: String,
    pub set: String,
    pub route: String,
}

pub struct MiyoriNetStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub uplink: Option<String>,
    pub tunnel: Option<TunnelBlock>,
}

// демон её не запускает и PID не хранит (решение C) — единственный способ найти miyori-net снаружи это /proc
pub fn miyori_net_status() -> MiyoriNetStatus {
    let proc_dir = Path::new("/proc");
    let Some((pid, cmdline)) = find_miyori_net(proc_dir) else {
        return MiyoriNetStatus {
            running: false,
            pid: None,
            uplink: None,
            tunnel: None,
        };
    };
    let tunnel = console_path_of(proc_dir, pid)
        .and_then(|p| read_console_tail(&p, CONSOLE_TAIL_BYTES))
        .and_then(|text| parse_last_tunnel_block(&text));
    MiyoriNetStatus {
        running: true,
        pid: Some(pid),
        uplink: parse_uplink(&cmdline),
        tunnel,
    }
}

fn find_miyori_net(proc_dir: &Path) -> Option<(u32, Vec<u8>)> {
    let entries = std::fs::read_dir(proc_dir).ok()?;
    for entry in entries.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(pid) => pid,
            None => continue,
        };
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        if cmdline_is_miyori_net(&cmdline) {
            return Some((pid, cmdline));
        }
    }
    None
}

// MIYORI_UPLINK в cmdline печатает только профиль miyori-net — обычные спейсы этот параметр не знают
fn cmdline_is_miyori_net(cmdline: &[u8]) -> bool {
    contains_subslice(cmdline, b"qemu-system-x86_64")
        && contains_subslice(cmdline, b"MIYORI_UPLINK=")
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// -append склеивает параметры ядра пробелами, а cmdline — argv через NUL; после замены NUL на пробел split_whitespace видит оба вида границ одинаково
fn parse_uplink(cmdline: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(cmdline).replace('\0', " ");
    text.split_whitespace()
        .find_map(|tok| tok.strip_prefix("MIYORI_UPLINK=").map(str::to_string))
}

// консоль недоверенная (решение H): is_file() отсеивает pipe/socket/tty и то, что уже удалено с диска
fn console_path_of(proc_dir: &Path, pid: u32) -> Option<PathBuf> {
    let target = std::fs::read_link(proc_dir.join(pid.to_string()).join("fd").join("1")).ok()?;
    (target.is_absolute() && target.is_file()).then_some(target)
}

// хвост, а не файл целиком — консоль miyori-net растёт неограниченно, пока она жива
fn read_console_tail(path: &Path, cap: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(cap))).ok()?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// последний блок побеждает — консоль видит все перезапуски miyori-net подряд, а не только первый
fn parse_last_tunnel_block(text: &str) -> Option<TunnelBlock> {
    const BEGIN: &str = "---MIYORI-TUNNEL-BEGIN---";
    const END: &str = "---MIYORI-TUNNEL-END---";

    let start = text.rfind(BEGIN)?;
    let body = &text[start + BEGIN.len()..];
    let stop = body.find(END)?;
    let body = &body[..stop];

    let mut ifc = None;
    let mut set = None;
    let mut route = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("TUNNEL-IFC:") {
            ifc = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("TUNNEL-SET:") {
            set = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("TUNNEL-ROUTE:") {
            route = Some(v.trim().to_string());
        }
    }
    Some(TunnelBlock {
        ifc: ifc?,
        set: set?,
        route: route?,
    })
}

pub struct RulesetStatus {
    pub available: bool,
    pub text: String,
    // сколько символов было до обрезки; None — не обрезали
    pub truncated_from: Option<usize>,
}

// читать nft умеет только root — оператор видит ruleset только через демона (спека §7)
pub fn ruleset_from_output(
    ok: bool,
    stdout: &[u8],
    stderr: &[u8],
    max_chars: usize,
) -> RulesetStatus {
    let raw = if ok {
        String::from_utf8_lossy(stdout).into_owned()
    } else {
        let reason = String::from_utf8_lossy(stderr);
        if reason.trim().is_empty() {
            "nft не отдал причину отказа".to_string()
        } else {
            reason.into_owned()
        }
    };

    let char_count = raw.chars().count();
    let (text, truncated_from) = if char_count > max_chars {
        (raw.chars().take(max_chars).collect(), Some(char_count))
    } else {
        (raw, None)
    };

    RulesetStatus {
        available: ok,
        text,
        truncated_from,
    }
}

pub fn nft_ruleset() -> RulesetStatus {
    match Command::new("nft").args(["list", "ruleset"]).output() {
        Ok(output) => ruleset_from_output(
            output.status.success(),
            &output.stdout,
            &output.stderr,
            RULESET_MAX_CHARS,
        ),
        Err(err) => ruleset_from_output(
            false,
            b"",
            format!("не удалось запустить nft: {err}").as_bytes(),
            RULESET_MAX_CHARS,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_and_bridge_names_come_from_shared_formula() {
        // сама сеть net.rs не трогает без root; здесь проверяем только то, что формулы не разошлись
        assert_eq!(crate::space_tap(7), "tap-space-7");
        assert_eq!(BRIDGE, "br-spaces");
        assert_eq!(CAPTIVE_BRIDGE, "br-captive");
    }

    #[test]
    fn link_exists_is_false_for_absent_and_true_for_lo() {
        assert!(!link_exists("miyorid-test-no-such-iface"));
        assert!(link_exists("lo"));
    }

    #[test]
    fn reap_leftover_tap_is_noop_when_interface_absent() {
        // cid без реального спейса даёт заведомо не существующее имя tap — без root проверяемо только это
        reap_leftover_tap(u32::MAX).unwrap();
    }

    #[test]
    fn write_atomically_leaves_no_tmp_file_behind() {
        let dir =
            std::env::temp_dir().join(format!("miyorid-net-test-{}-atomic", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spaces.toml");

        write_atomically(&path, "hello").unwrap();

        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["spaces.toml"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyorid-net-status-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_space_taps_at_finds_only_space_taps_and_reads_isolation() {
        let dir = temp_dir("taps");
        std::fs::create_dir_all(dir.join("lo")).unwrap();
        std::fs::create_dir_all(dir.join("tap-spaces")).unwrap(); // мост фикстуры, не спейс
        std::fs::create_dir_all(dir.join("tap-space-3").join("brport")).unwrap();
        std::fs::write(
            dir.join("tap-space-3").join("brport").join("isolated"),
            "1\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("tap-space-10")).unwrap(); // ещё не на мосту — brport нет

        let taps = list_space_taps_at(&dir);

        assert_eq!(taps.len(), 2);
        assert_eq!(taps[0].name, "tap-space-10");
        assert_eq!(taps[0].isolated, None);
        assert_eq!(taps[1].name, "tap-space-3");
        assert_eq!(taps[1].isolated, Some(true));
    }

    #[test]
    fn list_space_taps_at_on_missing_dir_is_empty_not_error() {
        let dir = temp_dir("taps-missing").join("does-not-exist");
        assert_eq!(list_space_taps_at(&dir).len(), 0);
    }

    #[test]
    fn parse_isolated_flag_reads_sysfs_boolean() {
        assert_eq!(parse_isolated_flag("0\n"), Some(false));
        assert_eq!(parse_isolated_flag("1\n"), Some(true));
        assert_eq!(parse_isolated_flag(""), None);
        assert_eq!(parse_isolated_flag("mусор\n"), None);
    }

    #[test]
    fn pci_status_at_reports_absent_device() {
        let dir = temp_dir("pci-absent");
        let status = pci_status_at(&dir, "0000:0c:00.0");
        assert!(!status.present);
        assert_eq!(status.driver, None);
    }

    #[test]
    fn pci_status_at_reports_present_without_driver() {
        let dir = temp_dir("pci-no-driver");
        std::fs::create_dir_all(dir.join("0000:0c:00.0")).unwrap();
        let status = pci_status_at(&dir, "0000:0c:00.0");
        assert!(status.present);
        assert_eq!(status.driver, None);
    }

    #[test]
    fn pci_status_at_reads_driver_from_symlink() {
        let dir = temp_dir("pci-driver");
        let dev = dir.join("0000:0c:00.0");
        std::fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink("../../../bus/pci/drivers/vfio-pci", dev.join("driver"))
            .unwrap();
        let status = pci_status_at(&dir, "0000:0c:00.0");
        assert!(status.present);
        assert_eq!(status.driver.as_deref(), Some("vfio-pci"));
    }

    #[test]
    fn cmdline_is_miyori_net_requires_both_markers() {
        let miyori_net = b"qemu-system-x86_64\0-append\0console=ttyS0 MIYORI_UPLINK=vfio\0";
        assert!(cmdline_is_miyori_net(miyori_net));

        let space = b"qemu-system-x86_64\0-device\0vhost-vsock-device,guest-cid=5\0";
        assert!(!cmdline_is_miyori_net(space));

        let foreign = b"sleep\x00300\0";
        assert!(!cmdline_is_miyori_net(foreign));
    }

    #[test]
    fn parse_uplink_reads_kernel_param_out_of_append_string() {
        let cmdline = b"qemu-system-x86_64\0-append\0console=ttyS0 root=/dev/vda rw MIYORI_UPLINK=vfio MIYORI_KILLSWITCH_TEST=none\0";
        assert_eq!(parse_uplink(cmdline), Some("vfio".to_string()));
    }

    #[test]
    fn parse_uplink_missing_is_none() {
        let cmdline = b"qemu-system-x86_64\0-append\0console=ttyS0\0";
        assert_eq!(parse_uplink(cmdline), None);
    }

    #[test]
    fn find_miyori_net_skips_foreign_and_non_numeric_entries() {
        let dir = temp_dir("proc-find");
        std::fs::create_dir_all(dir.join("self")).unwrap();
        std::fs::create_dir_all(dir.join("111")).unwrap();
        std::fs::write(dir.join("111").join("cmdline"), b"sleep\x00300\0").unwrap();
        std::fs::create_dir_all(dir.join("222")).unwrap();
        std::fs::write(
            dir.join("222").join("cmdline"),
            b"qemu-system-x86_64\0-append\0MIYORI_UPLINK=captive\0",
        )
        .unwrap();

        let found = find_miyori_net(&dir).expect("должен найти 222");
        assert_eq!(found.0, 222);
        assert_eq!(parse_uplink(&found.1), Some("captive".to_string()));
    }

    #[test]
    fn find_miyori_net_none_when_nobody_matches() {
        let dir = temp_dir("proc-find-none");
        std::fs::create_dir_all(dir.join("111")).unwrap();
        std::fs::write(dir.join("111").join("cmdline"), b"sleep\x00300\0").unwrap();
        assert!(find_miyori_net(&dir).is_none());
    }

    #[test]
    fn console_path_of_accepts_only_absolute_regular_file() {
        let dir = temp_dir("console-path");
        let real_file = dir.join("console.txt");
        std::fs::write(&real_file, "hello").unwrap();

        let fd_dir = dir.join("42").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink(&real_file, fd_dir.join("1")).unwrap();

        assert_eq!(console_path_of(&dir, 42), Some(real_file));
    }

    #[test]
    fn console_path_of_rejects_pipe_and_missing_target() {
        let dir = temp_dir("console-path-reject");

        let fd_dir = dir.join("7").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("pipe:[12345]", fd_dir.join("1")).unwrap();
        assert_eq!(console_path_of(&dir, 7), None);

        let fd_dir2 = dir.join("8").join("fd");
        std::fs::create_dir_all(&fd_dir2).unwrap();
        std::os::unix::fs::symlink(dir.join("gone.txt"), fd_dir2.join("1")).unwrap();
        assert_eq!(console_path_of(&dir, 8), None);
    }

    #[test]
    fn read_console_tail_caps_bytes_read() {
        let dir = temp_dir("console-tail");
        let path = dir.join("console.txt");
        let content = format!("{}TAIL", "x".repeat(1000));
        std::fs::write(&path, &content).unwrap();

        let tail = read_console_tail(&path, 4).unwrap();
        assert_eq!(tail, "TAIL");
    }

    #[test]
    fn read_console_tail_tolerates_invalid_utf8() {
        let dir = temp_dir("console-tail-binary");
        let path = dir.join("console.bin");
        std::fs::write(&path, [0xff, 0xfe, b'a', b'b']).unwrap();

        assert!(read_console_tail(&path, 64).is_some());
    }

    #[test]
    fn parse_last_tunnel_block_reads_the_three_fields() {
        let text = "\
noise before\n\
---MIYORI-TUNNEL-BEGIN---\n\
TUNNEL-IFC: tun_abcd\n\
TUNNEL-SET: { tun_abcd }\n\
TUNNEL-ROUTE: default dev tun_abcd table 100\n\
---MIYORI-TUNNEL-END---\n\
noise after\n";

        let block = parse_last_tunnel_block(text).unwrap();
        assert_eq!(block.ifc, "tun_abcd");
        assert_eq!(block.set, "{ tun_abcd }");
        assert_eq!(block.route, "default dev tun_abcd table 100");
    }

    #[test]
    fn parse_last_tunnel_block_picks_the_last_of_several() {
        let text = "\
---MIYORI-TUNNEL-BEGIN---\n\
TUNNEL-IFC: none\n\
TUNNEL-SET: \n\
TUNNEL-ROUTE: \n\
---MIYORI-TUNNEL-END---\n\
---MIYORI-TUNNEL-BEGIN---\n\
TUNNEL-IFC: tun0\n\
TUNNEL-SET: { tun0 }\n\
TUNNEL-ROUTE: default dev tun0 table 100\n\
---MIYORI-TUNNEL-END---\n";

        let block = parse_last_tunnel_block(text).unwrap();
        assert_eq!(block.ifc, "tun0");
    }

    #[test]
    fn parse_last_tunnel_block_none_when_truncated_mid_block() {
        let text = "\
---MIYORI-TUNNEL-BEGIN---\n\
TUNNEL-IFC: tun0\n\
TUNNEL-SET: { tun0 }\n";
        assert_eq!(parse_last_tunnel_block(text), None);
    }

    #[test]
    fn parse_last_tunnel_block_none_when_field_missing() {
        let text = "\
---MIYORI-TUNNEL-BEGIN---\n\
TUNNEL-IFC: tun0\n\
---MIYORI-TUNNEL-END---\n";
        assert_eq!(parse_last_tunnel_block(text), None);
    }

    #[test]
    fn parse_last_tunnel_block_none_when_absent() {
        assert_eq!(parse_last_tunnel_block("hello world\n"), None);
    }

    #[test]
    fn ruleset_from_output_success_returns_full_text() {
        let status = ruleset_from_output(true, b"table inet filter {}\n", b"", RULESET_MAX_CHARS);
        assert!(status.available);
        assert_eq!(status.text, "table inet filter {}\n");
        assert_eq!(status.truncated_from, None);
    }

    #[test]
    fn ruleset_from_output_failure_shows_reason_from_stderr() {
        let status =
            ruleset_from_output(false, b"", b"Operation not permitted\n", RULESET_MAX_CHARS);
        assert!(!status.available);
        assert!(status.text.contains("Operation not permitted"));
    }

    #[test]
    fn ruleset_from_output_failure_with_empty_stderr_is_not_blank() {
        let status = ruleset_from_output(false, b"", b"", RULESET_MAX_CHARS);
        assert!(!status.available);
        assert!(!status.text.trim().is_empty());
    }

    #[test]
    fn ruleset_from_output_truncates_by_char_count() {
        let long = "a".repeat(10);
        let status = ruleset_from_output(true, long.as_bytes(), b"", 4);
        assert_eq!(status.text.chars().count(), 4);
        assert_eq!(status.truncated_from, Some(10));
    }

    #[test]
    fn ruleset_from_output_invalid_utf8_does_not_panic() {
        let status = ruleset_from_output(true, &[0xff, 0xfe, b'a'], b"", RULESET_MAX_CHARS);
        assert!(status.available);
    }

    #[test]
    fn ruleset_from_output_truncates_multibyte_chars_by_char_not_byte() {
        let text = "привет".repeat(5);
        let char_count = text.chars().count();
        let status = ruleset_from_output(true, text.as_bytes(), b"", 4);
        assert_eq!(status.text, "прив");
        assert_eq!(status.text.chars().count(), 4);
        assert_eq!(status.truncated_from, Some(char_count));
    }
}
