#![forbid(unsafe_code)]

// argv — чистая функция отдельно от запуска, как qemu::build_argv: инварианты проверяются без setpriv и сети
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

// группы сбрасываются как в qemu::build_argv, но --no-new-privs здесь нет: он запретил бы setuid-root newuidmap, без которого rootless-сборки не существует
pub fn build_argv(build_uid: u32, build_gid: u32, profile_dir: &Path) -> Vec<String> {
    vec![
        "setpriv".into(),
        "--reuid".into(),
        build_uid.to_string(),
        "--regid".into(),
        build_gid.to_string(),
        "--clear-groups".into(),
        "--".into(),
        "bash".into(),
        "tools/build-profile.sh".into(),
        profile_dir.display().to_string(),
    ]
}

// mmdebstrap пишет прогресс в stderr, а не в stdout — оба потока обязаны кормить on_line, иначе progress молчит
pub fn run_streaming(
    argv: &[String],
    out_dir: &Path,
    home: &str,
    mut on_line: impl FnMut(&str),
) -> Result<ExitStatus> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env("MIYORI_OUT_DIR", out_dir)
        // setpriv меняет uid, а не HOME — без этого rustup сборщика лезет в чужой $HOME демона (root)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("не удалось запустить сборщик")?;

    let stdout = child.stdout.take().expect("stdout запрошен как piped");
    let stderr = child.stderr.take().expect("stderr запрошен как piped");
    // отдельные потоки-читатели: иначе заполненный буфер одного из них подвесит чтение другого
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let tx_stdout = tx.clone();
    let stdout_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            let _ = tx_stdout.send(line);
        }
    });
    // stderr одновременно копится в хвост для сообщения об ошибке и уходит в тот же канал, что и stdout
    let stderr_thread = std::thread::spawn(move || -> String {
        let mut tail = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            tail.push(line.clone());
            let _ = tx.send(line);
        }
        tail.join("\n")
    });

    for line in rx {
        on_line(&line);
    }

    let _ = stdout_thread.join();
    let status = child.wait().context("сборщик не завершился")?;
    let tail = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        anyhow::bail!("сборщик завершился с {status}: {tail}");
    }
    Ok(status)
}

// build-profile.sh на любом успехе обновляет latest — второй источник истины о digest не нужен
pub fn digest_from_out_dir(out_dir: &Path, profile: &str) -> Result<String> {
    let latest = out_dir.join(profile).join("latest");
    let target = std::fs::read_link(&latest)
        .with_context(|| format!("не читается символическая ссылка {}", latest.display()))?;
    target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .with_context(|| format!("некорректная ссылка {}", latest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use miyori_proto::control::{ErrorCode, Responder};
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-build-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --no-new-privs здесь намеренно отсутствует (см. комментарий над build_argv) — не забытый флаг
    #[test]
    fn privileges_are_dropped_but_setuid_helpers_stay_allowed() {
        let a = build_argv(70042, 70100, Path::new("/var/lib/miyorios/profiles/spike"));
        assert_eq!(a[0], "setpriv");
        assert_eq!(a[1], "--reuid");
        assert_eq!(a[2], "70042");
        assert_eq!(a[3], "--regid");
        assert_eq!(a[4], "70100");
        assert!(a.contains(&"--clear-groups".to_string()));
        assert!(
            !a.contains(&"--no-new-privs".to_string()),
            "с --no-new-privs setuid newuidmap/newgidmap не поднимут привилегии, и mmdebstrap --mode=unshare не запустится"
        );
        let sep = a
            .iter()
            .position(|s| s == "--")
            .expect("нет разделителя --");
        assert_eq!(a[sep + 1], "bash");
        assert_eq!(a[sep + 2], "tools/build-profile.sh");
        assert_eq!(a[sep + 3], "/var/lib/miyorios/profiles/spike");
    }

    #[test]
    fn run_streaming_delivers_lines_in_order() {
        let dir = temp_dir("order");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo one; echo two; echo three".to_string(),
        ];
        let mut seen = Vec::new();
        run_streaming(&argv, &dir, "/tmp", |line| seen.push(line.to_string())).unwrap();
        assert_eq!(seen, vec!["one", "two", "three"]);
    }

    // задача 10: HOME демона (root) сборщику не подходит — rustup уходит за тулчейном в чужой /root/.rustup
    #[test]
    fn run_streaming_sets_home_for_the_builder() {
        let dir = temp_dir("home-env");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo \"$HOME\"".to_string(),
        ];
        let mut seen = Vec::new();
        run_streaming(&argv, &dir, "/home/nonstandard", |line| {
            seen.push(line.to_string())
        })
        .unwrap();
        assert_eq!(seen, vec!["/home/nonstandard"]);
    }

    // mmdebstrap пишет прогресс исключительно в stderr — stdout-only чтение (старый баг) это бы пропустило
    #[test]
    fn run_streaming_delivers_stderr_only_lines_to_on_line() {
        let dir = temp_dir("stderr-only");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo строка >&2".to_string(),
        ];
        let mut seen = Vec::new();
        run_streaming(&argv, &dir, "/tmp", |line| seen.push(line.to_string())).unwrap();
        assert_eq!(seen, vec!["строка"]);
    }

    #[test]
    fn run_streaming_fails_on_nonzero_exit() {
        let dir = temp_dir("fail");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo boom >&2; exit 7".to_string(),
        ];
        let err = run_streaming(&argv, &dir, "/tmp", |_| {}).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn digest_from_out_dir_reads_latest_symlink() {
        let dir = temp_dir("digest-ok");
        let templates = dir.join("spike");
        std::fs::create_dir_all(templates.join("deadbeef")).unwrap();
        std::os::unix::fs::symlink("deadbeef", templates.join("latest")).unwrap();
        assert_eq!(digest_from_out_dir(&dir, "spike").unwrap(), "deadbeef");
    }

    #[test]
    fn digest_from_out_dir_errors_without_latest() {
        let dir = temp_dir("digest-missing");
        assert!(digest_from_out_dir(&dir, "spike").is_err());
    }

    // задача 2: прогресс обязан прийти ДО терминального кадра — тот же приём, которым ops::build() кормит Responder
    #[test]
    fn progress_frames_precede_the_terminal_frame() {
        let dir = temp_dir("responder-order");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo качаем; echo распаковываем".to_string(),
        ];
        let mut buf = Vec::new();
        let mut responder = Responder::new(&mut buf);
        let result = run_streaming(&argv, &dir, "/tmp", |line| {
            let _ = responder.progress(line);
        })
        .map(|_| serde_json::json!({"done": true}))
        .map_err(|err| (ErrorCode::Internal, err.to_string()));
        responder.finish(result).unwrap();

        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].get("progress").is_some(), "{text}");
        assert!(lines[1].get("progress").is_some(), "{text}");
        assert!(
            lines[2].get("ok").is_some(),
            "терминальный кадр должен быть последним: {text}"
        );
    }
}
