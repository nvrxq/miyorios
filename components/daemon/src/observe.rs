#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

// last-run.log может расти до гигабайт — тянуть в память нужно только хвост, а не весь файл
const TAIL_WINDOW_BYTES: u64 = 64 * 1024;
pub const LOG_TAIL_MAX_LINES: usize = 40;
pub const LOG_TAIL_MAX_LINE_CHARS: usize = 300;

pub fn parse_vm_rss_kib(status: &str) -> Option<u64> {
    for line in status.lines() {
        // strip_prefix требует точного совпадения вплоть до ':' — "VmRSSFoo:" не пройдёт
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

pub fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_vm_rss_kib(&status)
}

// возраст qemu.pid и есть возраст запуска: демон пишет его сразу после spawn (qemu::spawn)
pub fn uptime_secs(pid_path: &Path, now: SystemTime) -> Option<u64> {
    let mtime = std::fs::metadata(pid_path).ok()?.modified().ok()?;
    now.duration_since(mtime).ok().map(|d| d.as_secs())
}

// last-run.log — консоль гостя, гость пишет туда что хочет: нулевое доверие к содержимому
pub fn tail_sanitized(path: &Path, max_lines: usize, max_line_chars: usize) -> Vec<String> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }

    if start > 0 {
        // окно началось не с начала файла — первая строка обрезана посередине, показывать её нельзя
        match buf.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                buf.drain(..=idx);
            }
            None => return Vec::new(),
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let keep_from = lines.len().saturating_sub(max_lines);
    lines[keep_from..]
        .iter()
        .map(|line| sanitize_line(line, max_line_chars))
        .collect()
}

// ESC и прочие управляющие символы — иначе гость перекрашивает интерфейс менеджера своими ANSI-кодами
fn sanitize_line(line: &str, max_line_chars: usize) -> String {
    line.chars()
        .filter(|c| !c.is_control())
        .take(max_line_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyorid-observe-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_vm_rss_kib_reads_the_number() {
        let status = "Name:\tbash\nVmRSS:\t    2048 kB\nVmSwap:\t       0 kB\n";
        assert_eq!(parse_vm_rss_kib(status), Some(2048));
    }

    #[test]
    fn parse_vm_rss_kib_absent_line_is_none() {
        // у зомби строки VmRSS в /proc/<pid>/status нет вовсе
        let status = "Name:\tdefunct\nState:\tZ (zombie)\n";
        assert_eq!(parse_vm_rss_kib(status), None);
    }

    #[test]
    fn parse_vm_rss_kib_garbage_number_is_none() {
        let status = "VmRSS:\t    abc kB\n";
        assert_eq!(parse_vm_rss_kib(status), None);
    }

    #[test]
    fn parse_vm_rss_kib_does_not_match_similar_prefix() {
        let status = "VmRSSFoo:\t 999 kB\n";
        assert_eq!(parse_vm_rss_kib(status), None);
    }

    #[test]
    fn tail_sanitized_missing_file_is_empty() {
        let dir = temp_dir("missing-file");
        let path = dir.join("last-run.log");
        assert_eq!(tail_sanitized(&path, 10, 100), Vec::<String>::new());
    }

    #[test]
    fn tail_sanitized_short_file_keeps_order() {
        let dir = temp_dir("short-file");
        let path = dir.join("last-run.log");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(tail_sanitized(&path, 10, 100), vec!["one", "two", "three"]);
    }

    #[test]
    fn tail_sanitized_keeps_last_n_lines_in_order() {
        let dir = temp_dir("hundred-lines");
        let path = dir.join("last-run.log");
        let mut content = String::new();
        for i in 1..=100 {
            content.push_str(&format!("line-{i}\n"));
        }
        std::fs::write(&path, content).unwrap();
        let lines = tail_sanitized(&path, 10, 100);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line-91");
        assert_eq!(lines[9], "line-100");
    }

    #[test]
    fn tail_sanitized_strips_ansi_escape_sequences() {
        let dir = temp_dir("ansi");
        let path = dir.join("last-run.log");
        std::fs::write(&path, "\x1b[31mкрасный\x1b[0m\n").unwrap();
        let lines = tail_sanitized(&path, 10, 300);
        assert_eq!(lines, vec!["[31mкрасный[0m"]);
        assert!(!lines[0].chars().any(|c| c.is_control()));
    }

    #[test]
    fn tail_sanitized_strips_cr_and_nul() {
        let dir = temp_dir("cr-nul");
        let path = dir.join("last-run.log");
        std::fs::write(&path, "bad\r\x00line\n").unwrap();
        let lines = tail_sanitized(&path, 10, 300);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].chars().any(|c| c.is_control()));
    }

    #[test]
    fn tail_sanitized_truncates_long_line_by_chars() {
        let dir = temp_dir("long-line");
        let path = dir.join("last-run.log");
        let long_line: String = "x".repeat(5000);
        std::fs::write(&path, format!("{long_line}\n")).unwrap();
        let lines = tail_sanitized(&path, 10, 200);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), 200);
    }

    #[test]
    fn tail_sanitized_handles_files_over_the_tail_window() {
        let dir = temp_dir("big-file");
        let path = dir.join("last-run.log");
        let mut content = String::new();
        // ~200 КиБ — заведомо больше TAIL_WINDOW_BYTES, каждая строка размечена номером
        for i in 0..2000 {
            content.push_str(&format!("line-{i:04}-{}\n", "x".repeat(90)));
        }
        std::fs::write(&path, &content).unwrap();
        let lines = tail_sanitized(&path, 5, 300);
        assert_eq!(lines.len(), 5);
        for (offset, line) in lines.iter().enumerate() {
            let expected_i = 1995 + offset;
            assert!(
                line.starts_with(&format!("line-{expected_i:04}-")),
                "line {offset}: {line}"
            );
        }
    }

    #[test]
    fn tail_sanitized_invalid_utf8_does_not_panic() {
        let dir = temp_dir("invalid-utf8");
        let path = dir.join("last-run.log");
        let mut bytes = b"good line\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00]);
        bytes.extend_from_slice(b"\nmore\n");
        std::fs::write(&path, &bytes).unwrap();
        let lines = tail_sanitized(&path, 10, 300);
        assert!(!lines.is_empty());
    }

    #[test]
    fn uptime_secs_computes_elapsed_since_mtime() {
        let dir = temp_dir("uptime-past");
        let path = dir.join("qemu.pid");
        std::fs::write(&path, "123").unwrap();
        let now = SystemTime::now();
        let sixty_ago = now - std::time::Duration::from_secs(60);
        let file = File::open(&path).unwrap();
        file.set_modified(sixty_ago).unwrap();
        assert_eq!(uptime_secs(&path, now), Some(60));
    }

    #[test]
    fn uptime_secs_future_mtime_is_none() {
        let dir = temp_dir("uptime-future");
        let path = dir.join("qemu.pid");
        std::fs::write(&path, "123").unwrap();
        let now = SystemTime::now();
        let future = now + std::time::Duration::from_secs(60);
        let file = File::open(&path).unwrap();
        file.set_modified(future).unwrap();
        assert_eq!(uptime_secs(&path, now), None);
    }

    #[test]
    fn uptime_secs_missing_file_is_none() {
        let dir = temp_dir("uptime-missing");
        let path = dir.join("qemu.pid");
        assert_eq!(uptime_secs(&path, SystemTime::now()), None);
    }
}
