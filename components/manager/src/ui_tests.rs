#![forbid(unsafe_code)]

use super::*;
use gtk4::glib::translate::IntoGlib;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

fn spaces_json() -> Value {
    json!([
        {"id":"alpha","profile":"web","label":"personal","color":"#1971c2","state":"stopped","isolation-level":"standard","isolation-reason":""},
        {"id":"beta","profile":"dev","label":"work","color":"#2f9e44","state":"running","isolation-level":"reduced","isolation-reason":"Тестовый профиль с дополнительными возможностями"}
    ])
}

fn spaces() -> view::SpacesView {
    view::spaces_view(Ok(Reply::Ok(spaces_json())))
}

fn detail(id: &str) -> Value {
    json!({
        "id":id, "profile":if id == "beta" {"dev"} else {"web"}, "state":"running",
        "description":format!("Описание {id}"), "cid":42, "uid":1000, "created":"2026-09-23",
        "app":{"mode":"persistent","command":"/usr/bin/application"},
        "digest":"sha256:8a77317a593ed14b7e607ad42363e9df12c439488fe83522",
        "manifest-path":"/etc/miyorios/profiles/dev/manifest.toml",
        "network":{"via":"miyori-net"}, "isolation":{"level":"reduced","reason":"Тестовый профиль с дополнительными возможностями","gpu":"software"},
        "resources":{"memory-mb":2048,"data-mb":4096,"cpus":2,"disk-gb":16},
        "rss-kib":524288, "data-qcow2-bytes":123731968, "uptime-secs":7260,
        "encrypted":false, "clean-shutdown":true, "log-tail":[format!("Журнал {id}"), "Приложение запущено"]
    })
}

struct Fixture {
    spaces: Value,
    list_error: bool,
    profiles_error: bool,
    delay_detail: bool,
    action_error: bool,
    built: bool,
}

struct FakeDaemon {
    socket: PathBuf,
    fixture: Arc<Mutex<Fixture>>,
    requests: Arc<Mutex<Vec<Value>>>,
    hold_build: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeDaemon {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("miyori-ui-smoke-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let fixture = Arc::new(Mutex::new(Fixture {
            spaces: spaces_json(),
            list_error: false,
            profiles_error: false,
            delay_detail: false,
            action_error: false,
            built: false,
        }));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hold_build = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let server_fixture = fixture.clone();
        let server_requests = requests.clone();
        let server_hold = hold_build.clone();
        let server_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !server_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                let fixture = server_fixture.clone();
                let requests = server_requests.clone();
                let hold = server_hold.clone();
                let stop = server_stop.clone();
                workers.push(std::thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
                    let request: Value = serde_json::from_str(&line).unwrap();
                    requests.lock().unwrap().push(request.clone());
                    let op = request["op"].as_str().unwrap();
                    if op == "describe" && fixture.lock().unwrap().delay_detail { std::thread::sleep(Duration::from_millis(350)); }
                    if op == "build" {
                        for step in 0..300 {
                            let _ = writeln!(stream, "{}", json!({"progress":format!("Шаг {step}: подготовка файлов образа")}));
                        }
                        while hold.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(5)); }
                    }
                    let mut state = fixture.lock().unwrap();
                    let mut response = json!({"ok":true});
                    match op {
                        "list" if state.list_error => response = json!({"ok":false,"code":"unavailable","message":"тестовый отказ списка"}),
                        "list" => response["data"] = state.spaces.clone(),
                        "profiles" if state.profiles_error => response = json!({"ok":false,"code":"profiles-error","message":"тестовый отказ профилей"}),
                        "profiles" => response["data"] = json!([
                            {"profile":"web","template":true,"manifest-ok":true,"isolation-level":"standard","manifest-error":null},
                            {"profile":"dev","template":state.built,"manifest-ok":true,"isolation-level":"reduced","manifest-error":null},
                            {"profile":"broken","template":false,"manifest-ok":false,"isolation-level":null,"manifest-error":"invalid TOML"}
                        ]),
                        "describe" => response["data"] = detail(request["space"].as_str().unwrap()),
                        "net-status" => response["data"] = json!({
                            "bridges":{"br-spaces":true,"br-captive":false},
                            "taps":[{"name":"tap-beta","isolated":true}],
                            "uplink-pci":{"address":"0000:06:00.0","present":true,"driver":"vfio-pci"},
                            "miyori-net":{"running":true,"pid":1234,"uplink":"vfio","tunnel":{"ifc":"tun0","set":true,"route":true}},
                            "killswitch_counters":"недоступны с хоста",
                            "ruleset":{"available":true,"text":"table inet filter {}"}
                        }),
                        "create" => {
                            state.spaces.as_array_mut().unwrap().push(json!({
                                "id":request["space"],"profile":request["profile"],"label":request["label"],"color":request["color"],
                                "state":"stopped","isolation-level":"standard","isolation-reason":"","encrypted":request.get("passphrase").is_some()
                            }));
                        }
                        "start" if request["passphrase"] != "test-secret" => response = json!({"ok":false,"code":"wrong-passphrase","message":"неверный пароль"}),
                        "build" if state.action_error => response = json!({"ok":false,"code":"build-failed","message":"нет места на диске"}),
                        "build" => state.built = true,
                        _ => {}
                    }
                    let _ = writeln!(stream, "{response}");
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            socket,
            fixture,
            requests,
            hold_build,
            stop,
            thread: Some(thread),
        }
    }

    fn count(&self, op: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["op"] == op)
            .count()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.hold_build.store(false, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        std::fs::remove_dir_all(self.socket.parent().unwrap()).unwrap();
    }
}

#[track_caller]
fn spin_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        for _ in 0..32 {
            if !glib::MainContext::default().iteration(false) {
                break;
            }
        }
        if ready() {
            return;
        }
        assert!(Instant::now() < deadline, "GTK condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn settle() {
    let start = Instant::now();
    spin_until(|| start.elapsed() >= Duration::from_millis(160));
}

fn descendants(widget: &impl IsA<gtk4::Widget>) -> Vec<gtk4::Widget> {
    let mut result = Vec::new();
    let mut child = widget.first_child();
    while let Some(widget) = child {
        result.extend(descendants(&widget));
        child = widget.next_sibling();
        result.push(widget);
    }
    result
}

fn has_text(widget: &impl IsA<gtk4::Widget>, text: &str) -> bool {
    descendants(widget).iter().any(|w| {
        w.downcast_ref::<gtk4::Label>()
            .is_some_and(|label| label.text().contains(text))
    })
}

fn dialog(title: &str) -> Option<gtk4::Window> {
    gtk4::Window::list_toplevels()
        .into_iter()
        .filter_map(|w| w.downcast::<gtk4::Window>().ok())
        .find(|w| w.is_visible() && w.title().as_deref() == Some(title))
}

fn click(window: &gtk4::Window, label: &str) {
    let button = descendants(window)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk4::Button>().ok())
        .find(|b| b.label().as_deref() == Some(label) || has_text(b, label))
        .expect("button");
    assert!(button.is_sensitive());
    button.emit_clicked();
}

fn password_entry(window: &gtk4::Window) -> gtk4::Entry {
    descendants(window)
        .into_iter()
        .find_map(|w| w.downcast::<gtk4::Entry>().ok())
        .expect("password field")
}

fn screenshot(window: &impl IsA<gtk4::Window>, name: &str) {
    settle();
    let Some(dir) = std::env::var_os("MIYORI_SCREENSHOT_DIR") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    let window = window.as_ref();
    let snapshot = gtk4::Snapshot::new();
    gtk4::WidgetPaintable::new(Some(window)).snapshot(
        &snapshot,
        window.width() as f64,
        window.height() as f64,
    );
    let node = snapshot.to_node().expect("rendered window");
    let texture = window.renderer().unwrap().render_texture(&node, None);
    texture.save_to_png(PathBuf::from(dir).join(name)).unwrap();
}

#[test]
#[ignore = "requires an isolated Xvfb display; run tools/manager-ui-smoke.sh"]
fn gtk_refresh_preserves_selection_and_navigation() {
    let data_home =
        PathBuf::from(std::env::var_os("XDG_DATA_HOME").expect("run tools/manager-ui-smoke.sh"));
    assert!(
        data_home.starts_with("/tmp")
            && data_home
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("miyori-ui-smoke.")
    );
    std::fs::write(data_home.join("recently-used.xbel"), r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0" xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks" xmlns:mime="http://www.freedesktop.org/standards/shared-mime-info">
  <bookmark href="file:///tmp/seed.txt" added="2026-09-23T12:00:00Z" modified="2026-09-23T12:00:00Z" visited="2026-09-23T12:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="text/plain"/>
      <bookmark:applications><bookmark:application name="miyori-ui-test" exec="miyori-ui-test %u" modified="2026-09-23T12:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
</xbel>"#).unwrap();
    gtk4::init().expect("GTK display");
    assert!(!gtk4::RecentManager::default().items().is_empty());
    let errors = Rc::new(RefCell::new(Vec::new()));
    let captured = errors.clone();
    let css = gtk4::CssProvider::new();
    css.connect_parsing_error(move |_, _, error| captured.borrow_mut().push(error.to_string()));
    css.load_from_data(STYLE_CSS);
    assert!(
        errors.borrow().is_empty(),
        "CSS errors: {:?}",
        errors.borrow()
    );
    let app = gtk4::Application::new(None::<&str>, gio::ApplicationFlags::NON_UNIQUE);
    app.register(None::<&gio::Cancellable>).unwrap();
    let old = build_window(&app, PathBuf::from("/tmp/miyori-ui-test-no-daemon.sock"));
    render_spaces(&old, spaces());
    let beta = old.spaces_list.row_at_index(1).unwrap();
    old.spaces_list.select_row(Some(&beta));
    render_spaces(&old, spaces());
    assert_eq!(
        old.selected.borrow().as_deref(),
        Some("beta"),
        "background refresh changed selection"
    );
    assert_eq!(
        old.spaces_list.selected_row().unwrap(),
        beta,
        "widget identity changed"
    );
    old.show_net_page();
    let mut reordered = spaces_json();
    reordered.as_array_mut().unwrap().reverse();
    render_spaces(&old, view::spaces_view(Ok(Reply::Ok(reordered))));
    assert_eq!(old.spaces_list.row_at_index(0).unwrap(), beta);
    assert_eq!(old.selected.borrow().as_deref(), Some("beta"));
    render_spaces(&old, spaces());
    assert_eq!(old.pages.visible_child_name().as_deref(), Some("network"));
    old.window.close();
    drop(old);
    settle();

    let server = FakeDaemon::new();
    let ui = build_window(&app, server.socket.clone());
    spin_until(|| ui.details.root.is_visible());
    assert!(has_text(&ui.details.root, "Описание alpha"));
    let beta = ui.spaces_list.row_at_index(1).unwrap();
    ui.spaces_list.select_row(Some(&beta));
    spin_until(|| has_text(&ui.details.root, "Описание beta"));
    ui.details.stack.set_visible_child_name("resources");
    render_spaces(&ui, spaces());
    assert_eq!(
        ui.details.stack.visible_child_name().as_deref(),
        Some("resources")
    );
    ui.details.stack.set_visible_child_name("overview");
    screenshot(&ui.window, "01-spaces.png");
    ui.window.set_default_size(900, 700);
    screenshot(&ui.window, "02-spaces-narrow.png");
    assert!(ui.window.width() <= 900, "minimum width exceeds 900px");
    ui.window.set_default_size(1180, 780);

    ui.search.set_text("personal");
    spin_until(|| ui.selected.borrow().as_deref() == Some("alpha"));
    ui.search.set_text("nothing-found");
    spin_until(|| ui.selected.borrow().is_none());
    assert!(!ui.detail_header.is_visible());
    ui.search.set_text("");
    ui.running_only.set_active(true);
    spin_until(|| ui.selected.borrow().as_deref() == Some("beta"));
    ui.running_only.set_active(false);
    spin_until(|| ui.latest_detail.get() == 0);
    server.fixture.lock().unwrap().delay_detail = true;
    ui.spaces_list
        .select_row(ui.spaces_list.row_at_index(0).as_ref());
    let old_detail = ui.latest_detail.get();
    ui.spaces_list.select_row(Some(&beta));
    assert_eq!(
        ui.latest_detail.get(),
        old_detail,
        "selection spawned another in-flight read"
    );
    let next = ui.next_id.get();
    for _ in 0..100 {
        refresh_detail(&ui, "beta".to_string());
    }
    assert_eq!(ui.next_id.get(), next);
    spin_until(|| ui.details.root.is_visible() && has_text(&ui.details.root, "Описание beta"));
    server.fixture.lock().unwrap().delay_detail = false;

    refresh_detail(&ui, "beta".to_string());
    let stale = ui.latest_detail.get();
    render_spaces(
        &ui,
        view::SpacesView::Unavailable {
            message: "lost".to_string(),
        },
    );
    handle_done(&ui, stale, Ok(Reply::Ok(detail("beta"))));
    assert!(ui.selected.borrow().is_none());
    assert!(!ui.details.root.is_visible());
    assert!(!ui.create_button.is_sensitive());
    refresh_list(&ui);
    spin_until(|| ui.connected.get() && ui.details.root.is_visible());
    ui.show_images_page();
    spin_until(|| ui.profiles.borrow().len() == 3);
    let dev = ui.profile_list.row_at_index(1).unwrap();
    ui.profile_list.select_row(Some(&dev));
    request_profiles_for_dialog(&ui);
    spin_until(|| ui.latest_profiles.get() == 0);
    assert_eq!(ui.profile_list.selected_row().unwrap(), dev);
    assert_eq!(ui.selected_profile.borrow().as_deref(), Some("dev"));
    ui.profile_list
        .select_row(ui.profile_list.row_at_index(2).as_ref());
    assert!(!ui.profile_build.is_sensitive());
    assert!(ui.profile_info.text().contains("invalid TOML"));

    open_create_dialog(&ui);
    let form = ui.create_dialog.borrow().clone().unwrap();
    spin_until(|| form.profiles.borrow().len() == 3);
    form.id_entry.set_text("alpha");
    assert!(!form.can_create.get());
    form.id_entry.set_text("secure-work");
    form.profile_dropdown.set_selected(1);
    assert!(form.profile_build_button.is_visible());
    assert!(!form.can_create.get());
    form.profile_dropdown.set_selected(0);
    click(&form.window, "Выбрать");
    let chooser = form
        .seed_chooser
        .borrow()
        .clone()
        .expect("directory chooser retained");
    spin_until(|| chooser.current_folder().is_some());
    chooser
        .set_file(&gio::File::for_path(server.socket.parent().unwrap()))
        .unwrap();
    spin_until(|| chooser.file().and_then(|file| file.path()).as_deref() == server.socket.parent());
    chooser.emit_by_name::<()>("response", &[&gtk4::ResponseType::Accept.into_glib()]);
    assert_eq!(
        form.seed_entry.text().as_str(),
        server.socket.parent().unwrap().to_str().unwrap()
    );
    assert!(form.seed_chooser.borrow().is_none());
    form.color_checks[2].1.set_active(true);
    assert_eq!(effective_color(&form), PRESET_COLORS[2]);
    form.encrypt.set_active(true);
    assert!(
        !form.can_create.get(),
        "encryption silently accepted an empty password"
    );
    form.passphrase_entry.set_text("test-secret");
    form.passphrase_confirm_entry.set_text("mismatch");
    assert!(!form.can_create.get());
    form.passphrase_confirm_entry.set_text("test-secret");
    assert!(form.can_create.get());
    screenshot(&form.window, "03-create.png");
    form.passphrase_confirm_entry.grab_focus();
    screenshot(&form.window, "03-create-encryption.png");
    form.create_button.emit_clicked();
    assert!(form.passphrase_entry.text().is_empty());
    assert!(form.passphrase_confirm_entry.text().is_empty());
    spin_until(|| dialog("Спейс создан").is_some());
    let created = dialog("Спейс создан").unwrap();
    let start_before = server.count("start");
    click(&created, "Запустить");
    let password =
        dialog("Пароль спейса").expect("encrypted create/start must prompt before list refresh");
    assert_eq!(server.count("start"), start_before);
    let entry = password_entry(&password);
    entry.set_text("wrong");
    click(&password, "Подтвердить");
    assert!(entry.text().is_empty());
    spin_until(|| dialog("Пароль спейса").is_some());
    let retry = dialog("Пароль спейса").unwrap();
    assert!(has_text(&retry, "Пароль не подошёл"));
    let entry = password_entry(&retry);
    assert!(entry.text().is_empty());
    entry.set_text("test-secret");
    click(&retry, "Подтвердить");
    spin_until(|| ui.pending_action.borrow().is_none());
    assert!(!ui.operation_status.text().contains("wrong-passphrase"));

    server.hold_build.store(true, Ordering::Relaxed);
    ui.profile_list.select_row(Some(&dev));
    start_build(&ui, "dev".to_string());
    spin_until(|| !ui.progress_lines.borrow().is_empty());
    let operation_id = ui.pending_action.borrow().as_ref().unwrap().id;
    handle_progress(&ui, operation_id + 99, "FOREIGN PROGRESS");
    assert!(!ui.operation_status.text().contains("FOREIGN"));
    let counter = ui.next_id.get();
    ui.show_net_page();
    poll_refresh(&ui);
    let bounded = ui.next_id.get();
    for _ in 0..100 {
        poll_refresh(&ui);
    }
    assert_eq!(ui.next_id.get(), bounded, "polls accumulated read threads");
    assert!(bounded > counter, "busy mutation blocked reads");
    spin_until(|| ui.latest_net.get() == 0 && ui.latest_list.get() == 0);
    assert_eq!(ui.pages.visible_child_name().as_deref(), Some("network"));
    assert!(!ui.net_message.is_visible());
    assert!(has_text(&ui.net_rows_box, "tun0"));
    screenshot(&ui.window, "06-network.png");
    ui.show_images_page();
    screenshot(&ui.window, "04-build.png");
    assert!(ui.progress_lines.borrow().len() <= 200);
    assert!(!ui.profile_build.is_sensitive());
    server.hold_build.store(false, Ordering::Relaxed);
    spin_until(|| ui.pending_action.borrow().is_none());
    spin_until(|| {
        ui.profiles
            .borrow()
            .iter()
            .any(|p| p.profile == "dev" && p.template)
    });
    assert!(ui.operation_status.text().contains("выполнено"));
    server.fixture.lock().unwrap().action_error = true;
    start_build(&ui, "dev".to_string());
    spin_until(|| ui.pending_action.borrow().is_none());
    assert!(ui
        .operation_status
        .text()
        .contains("build-failed: нет места на диске"));
    assert!(ui.operation_status.has_css_class("status-error"));

    open_create_dialog(&ui);
    let cancelled = ui.create_dialog.borrow().clone().unwrap();
    spin_until(|| !cancelled.profiles.borrow().is_empty());
    cancelled.encrypt.set_active(true);
    cancelled.passphrase_entry.set_text("do-not-retain");
    cancelled.encrypt.set_active(false);
    assert!(cancelled.passphrase_entry.text().is_empty());
    cancelled.window.close();
    assert!(ui.create_dialog.borrow().is_none());
    server.fixture.lock().unwrap().profiles_error = true;
    request_profiles_for_dialog(&ui);
    spin_until(|| ui.profile_message.text().contains("profiles-error"));
    assert!(!ui.profile_build.is_sensitive());
    ui.show_detail_page();
    server.fixture.lock().unwrap().spaces = json!([]);
    refresh_list(&ui);
    spin_until(|| ui.current_spaces.borrow().is_empty());
    assert!(ui.create_button.is_sensitive());
    assert!(ui.selected.borrow().is_none());
    server.fixture.lock().unwrap().list_error = true;
    refresh_list(&ui);
    spin_until(|| !ui.connected.get());
    assert!(ui
        .list_message
        .text()
        .contains("unavailable: тестовый отказ списка"));
    screenshot(&ui.window, "05-unavailable.png");
    open_create_dialog(&ui);
    let child = ui.create_dialog.borrow().clone().unwrap();
    child.encrypt.set_active(true);
    child.passphrase_entry.set_text("clear-on-parent-close");
    click(&child.window, "Выбрать");
    assert!(child.seed_chooser.borrow().is_some());
    ui.window.close();
    assert!(child.passphrase_entry.text().is_empty());
    assert!(child.seed_chooser.borrow().is_none());
    let weak = Rc::downgrade(&ui);
    drop(ui);
    settle();
    assert!(weak.upgrade().is_none(), "closed manager retained UI state");
}
