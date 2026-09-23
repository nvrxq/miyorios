use std::process::Command;

// каталог уникален на вызов: тесты Rust идут параллельно и делили бы один файл
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn run(manifest: &str) -> String {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("miyori-export-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("manifest.toml");
    std::fs::write(&path, manifest).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_miyori-profile"))
        .args(["export-env", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

const MANIFEST: &str = r#"
id = "telegram"
description = "Telegram"
[base]
builder = "mmdebstrap"
suite = "noble"
sources = ["deb http://archive.ubuntu.com/ubuntu noble main universe"]
packages = ["telegram-desktop", "ca-certificates"]
[app]
mode = "app"
command = "/usr/bin/telegram-desktop"
[resources]
memory_mb = 1024
cpus = 1
disk_gb = 8
data_mb = 4096
[isolation]
level = "standard"
gpu = "none"
[network]
via = "miyori-net"
"#;

#[test]
fn packages_are_comma_joined_for_mmdebstrap() {
    assert!(run(MANIFEST).contains("MIYORI_PACKAGES='telegram-desktop,ca-certificates'"));
}

#[test]
fn sources_are_exported_one_per_key() {
    let out = run(MANIFEST);
    assert!(out.contains("MIYORI_SOURCE_COUNT='1'"));
    assert!(
        out.contains("MIYORI_SOURCE_0='deb http://archive.ubuntu.com/ubuntu noble main universe'")
    );
}

#[test]
fn absent_keyring_yields_empty_value_not_missing_key() {
    let out = run(MANIFEST);
    assert!(out.contains("MIYORI_KEYRING_PATH=''"));
}

#[test]
fn blob_count_is_zero_without_blobs() {
    assert!(run(MANIFEST).contains("MIYORI_BLOB_COUNT='0'"));
}

#[test]
fn file_count_is_zero_without_files() {
    assert!(run(MANIFEST).contains("MIYORI_FILE_COUNT='0'"));
}

#[test]
fn file_fields_are_exported_by_index() {
    let src = format!(
        "{MANIFEST}\n[[base.file]]\npath = \"blobs/x.bin\"\nsha256 = \"{}\"\ninto = \"/opt/x\"\nmode = \"0755\"\n",
        "a".repeat(64)
    );
    let out = run(&src);
    assert!(out.contains("MIYORI_FILE_COUNT='1'"));
    assert!(out.contains("MIYORI_FILE_0_PATH='blobs/x.bin'"));
    assert!(out.contains(&format!("MIYORI_FILE_0_SHA256='{}'", "a".repeat(64))));
    assert!(out.contains("MIYORI_FILE_0_INTO='/opt/x'"));
    assert!(out.contains("MIYORI_FILE_0_MODE='0755'"));
}

#[test]
fn data_field_is_exported_in_mebibytes_not_gibibytes() {
    let out = run(MANIFEST);
    assert!(
        out.contains("MIYORI_DATA_MB="),
        "ключ MIYORI_DATA_MB отсутствует: {out}"
    );
    assert!(
        !out.contains("MIYORI_DATA_GB="),
        "старый ключ MIYORI_DATA_GB не должен оставаться: {out}"
    );
}

#[test]
fn single_quotes_in_values_are_escaped() {
    // проверять надо на экспортируемом поле: description наружу не идёт, и тест бы не смог провалиться
    // apostrophe в TOML-строке экранировать не нужно — сам TOML это разрешает без escape-последовательностей
    let src = MANIFEST.replace(r#""telegram-desktop", "ca-certificates""#, r#""it's-pkg""#);
    let out = run(&src);
    assert!(
        out.contains(r"MIYORI_PACKAGES='it'\''s-pkg'"),
        "кавычка не экранирована: {out}"
    );
}

#[test]
fn invalid_manifest_exits_nonzero() {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("miyori-export-bad-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("manifest.toml");
    std::fs::write(
        &path,
        MANIFEST.replace(r#"via = "miyori-net""#, r#"via = "direct""#),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_miyori-profile"))
        .args(["export-env", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
