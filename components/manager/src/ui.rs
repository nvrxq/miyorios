#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use miyori_proto::client::Reply;

use crate::daemon::{Daemon, Update};
use crate::view;

#[path = "widgets.rs"]
mod widgets;

// таблица стилей рядом; правила там, здесь только подключение
const STYLE_CSS: &str = include_str!("style.css");

// провайдер вешается на дисплей, а не на окно — так его подхватят и диалоги (отдельные gtk4::Window)
fn apply_style() {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    // GTK 4.14's recent-folder queries can outlive their model when a chooser closes immediately.
    gtk4::Settings::for_display(&display).set_gtk_recent_files_enabled(false);
    let provider = gtk4::CssProvider::new();
    // load_from_string требует feature "v4_12", которая не включена — load_from_data доступен без неё
    provider.load_from_data(STYLE_CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

// гамма фиксирована и не зависит от темы стола — это часть опознания недоверенного окружения
fn apply_dialog_style(dialog: &gtk4::Window) {
    dialog.add_css_class("dialog");
    dialog.connect_close_request(|window| {
        // GTK 4.14 can retain a stale transient pointer when a closed dialog outlives its parent.
        window.set_transient_for(None::<&gtk4::Window>);
        glib::Propagation::Proceed
    });
}

pub fn run(socket: PathBuf) -> i32 {
    let app = gtk4::Application::new(None::<&str>, gio::ApplicationFlags::empty());
    app.connect_activate(move |app| {
        build_window(app, socket.clone());
    });
    app.run_with_args(&["miyori-manager"]).into()
}

// после create нужно знать id нового спейса, чтобы выбрать его и предложить запуск — остальным операциям это не нужно
enum PendingKind {
    Create { new_id: String, encrypted: bool },
    // op и space нужны, чтобы переспросить пароль тем же диалогом на wrong-passphrase, не отправляя запрос заново вслепую
    Passphrase { op: &'static str, space: String },
    Other,
}

struct PendingAction {
    id: u64,
    label: String,
    kind: PendingKind,
    started: Instant,
}

struct SpaceWidgets {
    row: gtk4::ListBoxRow,
    title: gtk4::Label,
    subtitle: gtk4::Label,
    color: Rc<Cell<(f64, f64, f64)>>,
    bar: gtk4::DrawingArea,
    icon: gtk4::Image,
}

struct Ui {
    daemon: Daemon,
    tx: SyncSender<Update>,
    next_id: Cell<u64>,
    window: gtk4::ApplicationWindow,
    status_label: gtk4::Label,
    connection_label: gtk4::Label,
    pages: gtk4::Stack,
    navigation: [gtk4::ToggleButton; 3],
    search: gtk4::SearchEntry,
    all_filter: gtk4::ToggleButton,
    running_only: gtk4::ToggleButton,
    reconciling: Cell<bool>,
    space_widgets: RefCell<HashMap<String, SpaceWidgets>>,
    details: widgets::Details,
    detail_header: gtk4::Box,
    selection_epoch: Cell<u64>,
    detail_request: RefCell<Option<(String, u64)>>,
    connected: Cell<bool>,
    closed: Cell<bool>,

    profiles: RefCell<Vec<view::ProfileRow>>,
    profile_list: gtk4::ListBox,
    selected_profile: RefCell<Option<String>>,
    profile_message: gtk4::Label,
    profile_title: gtk4::Label,
    profile_info: gtk4::Label,
    profile_build: gtk4::Button,
    operation_panel: gtk4::Box,
    operation_title: gtk4::Label,
    operation_status: gtk4::Label,
    operation_clock: gtk4::Label,
    operation_spinner: gtk4::Spinner,
    operation_log: gtk4::TextView,
    progress_lines: RefCell<VecDeque<String>>,

    list_message: gtk4::Label,
    spaces_list: gtk4::ListBox,
    current_spaces: RefCell<Vec<view::SpaceRow>>,

    detail_message: gtk4::Label,
    log_view: gtk4::TextView,

    net_message: gtk4::Label,
    net_rows_box: gtk4::Box,
    net_note_label: gtk4::Label,
    ruleset_view: gtk4::TextView,

    create_button: gtk4::Button,
    start_button: gtk4::Button,
    stop_button: gtk4::Button,
    open_window_button: gtk4::Button,
    build_button: gtk4::Button,
    update_image_button: gtk4::Button,
    reset_system_button: gtk4::Button,
    reset_all_button: gtk4::Button,
    destroy_button: gtk4::Button,

    selected: RefCell<Option<String>>,
    // id только что созданного спейса — следующий render_spaces выберет его, даже если старый выбор ещё жив
    pending_select: RefCell<Option<String>>,
    latest_list: Cell<u64>,
    list_again: Cell<bool>,
    profiles_again: Cell<bool>,
    last_net: RefCell<Option<Result<view::NetView, String>>>,
    latest_detail: Cell<u64>,
    latest_net: Cell<u64>,
    latest_profiles: Cell<u64>,
    pending_action: RefCell<Option<PendingAction>>,
    create_dialog: RefCell<Option<Rc<CreateDialog>>>,
}

impl Ui {
    fn next_request_id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }

    fn sync_action_sensitivity(&self) {
        let idle = self.pending_action.borrow().is_none();
        let selected = self.selected.borrow().clone();
        let state = selected.as_ref().and_then(|id| {
            self.current_spaces
                .borrow()
                .iter()
                .find(|row| &row.id == id)
                .map(|row| row.state.clone())
        });
        let state = state.as_deref();
        self.start_button
            .set_visible(!matches!(state, Some("running") | Some("unresponsive")));
        self.open_window_button
            .set_visible(state == Some("running"));
        self.stop_button
            .set_visible(matches!(state, Some("running") | Some("unresponsive")));
        for (button, action) in [
            (&self.start_button, Action::Start),
            (&self.stop_button, Action::Stop),
            (&self.open_window_button, Action::OpenWindow),
            (&self.build_button, Action::Build),
            (&self.update_image_button, Action::UpdateImage),
            (&self.reset_system_button, Action::ResetSystem),
            (&self.reset_all_button, Action::ResetAll),
            (&self.destroy_button, Action::Destroy),
        ] {
            button.set_sensitive(idle && self.connected.get() && action_enabled(action, state));
        }
        self.create_button
            .set_sensitive(idle && self.connected.get());
        let profile = self.selected_profile.borrow().clone();
        self.profile_build.set_sensitive(
            idle && self.connected.get()
                && profile.as_ref().is_some_and(|id| {
                    self.profiles
                        .borrow()
                        .iter()
                        .any(|p| &p.profile == id && p.manifest_ok)
                }),
        );
        if let Some(dialog) = self.create_dialog.borrow().as_ref() {
            dialog
                .create_button
                .set_sensitive(idle && self.connected.get() && dialog.can_create.get());
            dialog
                .profile_build_button
                .set_sensitive(idle && self.connected.get() && dialog.can_build.get());
        }
    }

    fn show_detail_page(&self) {
        self.pages.set_visible_child_name("spaces");
        self.navigation[0].set_active(true);
    }

    fn show_net_page(&self) {
        self.pages.set_visible_child_name("network");
        self.navigation[2].set_active(true);
    }

    fn show_images_page(&self) {
        self.pages.set_visible_child_name("images");
        self.navigation[1].set_active(true);
    }
}

// значение вне #rrggbb — не повод падать: демон уже мог прислать мусор, полоска просто станет серой
fn parse_hex_color(color: &str) -> (f64, f64, f64) {
    const FALLBACK: (f64, f64, f64) = (0.5, 0.5, 0.5);
    let hex = match color.strip_prefix('#') {
        Some(hex) => hex,
        None => return FALLBACK,
    };
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return FALLBACK;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
    (
        channel(0) as f64 / 255.0,
        channel(2) as f64 / 255.0,
        channel(4) as f64 / 255.0,
    )
}

// причине уровня тут не место (спека Б) — она длинная и ломает однострочность; ей место в деталях
fn space_row_text(row: &view::SpaceRow) -> String {
    format!(
        "{} · {} · {} ({})",
        row.id, row.state, row.profile, row.level
    )
}

// GTK-класс без цвета — красит дизайнер отдельным проходом по style.css
fn state_css_class(state: &str) -> &'static str {
    match state {
        "running" => "state-running",
        "unresponsive" => "state-unresponsive",
        "stopped" => "state-stopped",
        _ => "state-unknown",
    }
}

const PRESET_LABELS: [&str; 5] = ["untrusted", "work", "personal", "banking", "testing"];

// заведомо различимые оттенки по кругу — цвет спейса должен различаться на глаз, а не подбираться руками
const PRESET_COLORS: [&str; 8] = [
    "#e03131", "#f08c00", "#2f9e44", "#0ca678", "#1971c2", "#4263eb", "#9c36b5", "#e64980",
];

// правило спрашивается у того же типа, который проверяет демон: разошедшаяся копия
// стала бы врать о готовности до первого настоящего запроса
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    OpenWindow,
    Build,
    ResetSystem,
    ResetAll,
    UpdateImage,
    Destroy,
}

// демон отвергает start у работающего, stop у остановленного и reset/destroy у живого:
// кнопка, которая гарантированно даст отказ, — это ловушка, а не возможность
fn action_enabled(action: Action, state: Option<&str>) -> bool {
    match action {
        // образ собирается по профилю, а не по спейсу: без выбора он тоже нужен, иначе первый спейс не завести
        Action::Build => true,
        Action::Start => state == Some("stopped"),
        Action::Stop => matches!(state, Some("running") | Some("unresponsive")),
        // у остановленного спейса демон гарантированно откажет — не предлагаем ловушку
        Action::OpenWindow => state == Some("running"),
        // подменять backing живого QEMU — порча данных, тот же гвард, что у reset-операций
        Action::ResetSystem | Action::ResetAll | Action::UpdateImage | Action::Destroy => {
            state == Some("stopped")
        }
    }
}

// зашифрованный спейс требует пароль ровно у четырёх операций, что открывают или пересоздают шифрованные тома
fn needs_passphrase(op: &str, spaces: &[view::SpaceRow], id: &str) -> bool {
    matches!(op, "start" | "reset-system" | "reset-all" | "update-image")
        && spaces.iter().any(|row| row.id == id && row.encrypted)
}

fn slug_error(candidate: &str) -> Option<String> {
    if candidate.is_empty() {
        return Some("не может быть пустым".to_string());
    }
    if miyori_proto::ids::SpaceId::new(candidate).is_err() {
        return Some("только строчные латинские буквы, цифры и дефис, не длиннее 32".to_string());
    }
    None
}

fn id_error(candidate: &str, existing_ids: &[String]) -> Option<String> {
    if let Some(reason) = slug_error(candidate) {
        return Some(reason);
    }
    if existing_ids.iter().any(|id| id == candidate) {
        return Some(format!("спейс «{candidate}» уже существует"));
    }
    None
}

// зеркалит miyori_proto::ids::is_rrggbb — тот же формат демон примет без ошибки на Color::new
fn is_rrggbb(candidate: &str) -> bool {
    miyori_proto::ids::Color::new(candidate).is_ok()
}

fn color_error(candidate: &str) -> Option<String> {
    if is_rrggbb(candidate) {
        None
    } else {
        Some("цвет должен быть в формате #rrggbb".to_string())
    }
}

// оба пусты — без шифрования, как раньше; иначе обязаны совпасть, иначе решение о шифровании принято молча
fn passphrase_error(passphrase: &str, confirm: &str) -> Option<String> {
    if passphrase.is_empty() && confirm.is_empty() {
        return None;
    }
    if passphrase != confirm {
        return Some("пароль и подтверждение не совпадают".to_string());
    }
    None
}

enum ProfileEligibility {
    Ready,
    NoTemplate,
    ManifestBroken(String),
}

// сломанный манифест важнее отсутствия образа: собрать образ по нечитаемому манифесту всё равно не выйдет
fn profile_eligibility(row: &view::ProfileRow) -> ProfileEligibility {
    if !row.manifest_ok {
        return ProfileEligibility::ManifestBroken(row.manifest_error.clone().unwrap_or_default());
    }
    if !row.template {
        return ProfileEligibility::NoTemplate;
    }
    ProfileEligibility::Ready
}

fn profile_option_text(row: &view::ProfileRow) -> String {
    match profile_eligibility(row) {
        ProfileEligibility::Ready => {
            format!(
                "{} ({})",
                row.profile,
                row.isolation_level.as_deref().unwrap_or("—")
            )
        }
        ProfileEligibility::NoTemplate => format!(
            "{} ({}) — образ не собран",
            row.profile,
            row.isolation_level.as_deref().unwrap_or("—")
        ),
        ProfileEligibility::ManifestBroken(_) => format!("{} — манифест не читается", row.profile),
    }
}

// сборку без выбранного спейса не выдумать профиль из ничего — только найти его у уже известного спейса
fn profile_for_build(spaces: &[view::SpaceRow], selected_id: &str) -> Option<String> {
    spaces
        .iter()
        .find(|space| space.id == selected_id)
        .map(|space| space.profile.clone())
}

// пустой ввод не повод бить демон запросом build без profile — тихо отказываем на этом этапе
fn normalize_profile_input(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

enum ConfirmKind {
    Destroy,
    ResetAll,
    ResetSystem,
    UpdateImage,
}

// цену необратимости называем прямо в тексте — только destroy и reset-all её несут
fn confirm_message(kind: &ConfirmKind, space_id: &str) -> String {
    match kind {
        ConfirmKind::Destroy => {
            format!("Удалить спейс «{space_id}» безвозвратно? Действие необратимо.")
        }
        ConfirmKind::ResetAll => format!(
            "Сбросить спейс «{space_id}» полностью — систему и данные? Действие необратимо. \
             Новый /data будет пустым: повторного засева файлами не будет."
        ),
        ConfirmKind::ResetSystem => {
            format!("Сбросить системный слой спейса «{space_id}»? Данные спейса не затрагиваются.")
        }
        ConfirmKind::UpdateImage => format!(
            "Обновить образ спейса «{space_id}» на текущий шаблон профиля? Всё, что установлено \
             в систему вручную поверх образа, будет потеряно. Том /data не затрагивается."
        ),
    }
}

// живёт, пока открыт диалог создания; сложность здесь оправдана требованием живой валидации и сборки без блокировки окна
struct CreateDialog {
    window: gtk4::Window,
    seed_chooser: RefCell<Option<gtk4::FileChooserNative>>,
    encrypt: gtk4::Switch,
    password_fields: gtk4::Box,
    preview_title: gtk4::Label,
    preview_subtitle: gtk4::Label,
    preview_color: Rc<Cell<(f64, f64, f64)>>,
    preview_bar: gtk4::DrawingArea,
    profiles: RefCell<Vec<view::ProfileRow>>,
    profile_dropdown: gtk4::DropDown,
    profile_hint: gtk4::Label,
    profile_build_button: gtk4::Button,
    status_label: gtk4::Label,
    id_entry: gtk4::Entry,
    id_error_label: gtk4::Label,
    label_combo: gtk4::ComboBoxText,
    label_error_label: gtk4::Label,
    color_checks: Vec<(String, gtk4::ToggleButton)>,
    color_manual_entry: gtk4::Entry,
    color_error_label: gtk4::Label,
    seed_entry: gtk4::Entry,
    passphrase_entry: gtk4::Entry,
    passphrase_confirm_entry: gtk4::Entry,
    passphrase_error_label: gtk4::Label,
    create_button: gtk4::Button,
    can_create: Cell<bool>,
    can_build: Cell<bool>,
}

fn selected_profile(dlg: &CreateDialog) -> Option<view::ProfileRow> {
    let index = usize::try_from(dlg.profile_dropdown.selected()).ok()?;
    dlg.profiles.borrow().get(index).cloned()
}

// ручной ввод побеждает пресет, только если в нём вообще что-то набрано
fn effective_color(dlg: &CreateDialog) -> String {
    let manual = dlg.color_manual_entry.text().trim().to_string();
    if !manual.is_empty() {
        return manual;
    }
    dlg.color_checks
        .iter()
        .find(|(_, check)| check.is_active())
        .map(|(hex, _)| hex.clone())
        .unwrap_or_else(|| PRESET_COLORS[0].to_string())
}

fn build_window(app: &gtk4::Application, socket: PathBuf) -> Rc<Ui> {
    let daemon = Daemon::new(socket);
    let (tx, rx) = sync_channel::<Update>(128);
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("MiyoriOS")
        .default_width(1180)
        .default_height(780)
        .build();
    apply_style();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.add_css_class("app-header");
    header.append(&widgets::label("miyori", "wordmark"));
    let navigation = ["Спейсы", "Образы", "Сеть"].map(gtk4::ToggleButton::with_label);
    for (i, button) in navigation.iter().enumerate() {
        button.add_css_class("nav-tab");
        if i > 0 {
            button.set_group(Some(&navigation[0]));
        }
        header.append(button);
    }
    navigation[0].set_active(true);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);
    let create_button = widgets::button("Создать спейс", "list-add-symbolic", true);
    header.append(&create_button);
    root.append(&header);
    let pages = gtk4::Stack::new();
    pages.set_vexpand(true);
    pages.set_hhomogeneous(false);
    pages.set_vhomogeneous(false);
    root.append(&pages);

    let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    sidebar.add_css_class("sidebar");
    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Поиск спейса"));
    sidebar.append(&search);
    let filters = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filters.add_css_class("filter-row");
    let all = gtk4::ToggleButton::with_label("Все");
    let running_only = gtk4::ToggleButton::with_label("Работают");
    running_only.set_group(Some(&all));
    all.set_active(true);
    for button in [&all, &running_only] {
        button.add_css_class("filter-button");
        filters.append(button);
    }
    sidebar.append(&filters);
    let list_message = widgets::label("Загрузка спейсов…", "status-message");
    list_message.set_wrap(true);
    sidebar.append(&list_message);
    let spaces_list = gtk4::ListBox::new();
    spaces_list.add_css_class("sidebar-list");
    sidebar.append(&widgets::scroll(&spaces_list));

    let template = match view::detail_view(Ok(Reply::Ok(serde_json::json!({})))) {
        view::DetailView::Space { rows, .. } => rows,
        _ => Vec::new(),
    };
    let details = widgets::Details::new(&template);
    let detail_page = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    detail_page.add_css_class("content");
    let detail_message = widgets::label("Загрузка спейсов…", "status-message");
    detail_message.set_wrap(true);
    detail_page.append(&detail_message);
    let detail_header = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    detail_header.add_css_class("space-header");
    let hero = gtk4::Box::new(gtk4::Orientation::Horizontal, 18);
    details.icon.set_pixel_size(38);
    details.icon.add_css_class("hero-icon");
    hero.append(&details.icon);
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    titles.set_hexpand(true);
    details.title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    details
        .subtitle
        .set_ellipsize(gtk4::pango::EllipsizeMode::End);
    titles.append(&details.title);
    titles.append(&details.subtitle);
    hero.append(&titles);
    detail_header.append(&hero);
    let badges = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    for badge in [&details.state, &details.level, &details.identity] {
        badges.append(badge);
    }
    details
        .identity
        .set_ellipsize(gtk4::pango::EllipsizeMode::End);
    detail_header.append(&badges);
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let start_button = widgets::button("Запустить", "media-playback-start-symbolic", true);
    let open_window_button = widgets::button("Открыть окно", "window-new-symbolic", true);
    let stop_button = widgets::button("Остановить", "media-playback-stop-symbolic", false);
    for button in [&start_button, &open_window_button, &stop_button] {
        actions.append(button);
    }
    let overflow = gtk4::MenuButton::new();
    overflow.set_icon_name("view-more-symbolic");
    overflow.set_tooltip_text(Some("Обслуживание спейса"));
    overflow.add_css_class("action-button");
    let popover = gtk4::Popover::new();
    let extra = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    extra.add_css_class("overflow-actions");
    let build_button = widgets::button("Собрать образ", "", false);
    let update_image_button = widgets::button("Обновить образ", "", false);
    let reset_system_button = widgets::button("Сбросить систему", "", false);
    let reset_all_button = widgets::button("Сбросить всё", "", false);
    let destroy_button = widgets::button("Удалить спейс", "", false);
    reset_system_button.add_css_class("action-caution");
    for button in [&reset_all_button, &destroy_button] {
        button.add_css_class("destructive-action");
    }
    for button in [
        &build_button,
        &update_image_button,
        &reset_system_button,
        &reset_all_button,
        &destroy_button,
    ] {
        extra.append(button);
        let popover = popover.clone();
        button.connect_clicked(move |_| popover.popdown());
    }
    popover.set_child(Some(&extra));
    overflow.set_popover(Some(&popover));
    actions.append(&overflow);
    detail_header.append(&actions);
    detail_page.append(&detail_header);
    detail_page.append(&details.root);
    detail_header.set_visible(false);
    details.root.set_visible(false);
    let log_view = details.log.clone();
    let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
    paned.set_position(270);
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&widgets::scroll(&detail_page)));
    pages.add_named(&paned, Some("spaces"));

    let profile_sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    profile_sidebar.add_css_class("sidebar");
    profile_sidebar.append(&widgets::label("Профили", "sidebar-title"));
    let profile_list = gtk4::ListBox::new();
    profile_list.add_css_class("sidebar-list");
    profile_sidebar.append(&widgets::scroll(&profile_list));
    let images = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    images.add_css_class("content");
    images.append(&widgets::label("Образы", "page-title"));
    images.append(&widgets::label(
        "Профили и сборки для ваших спейсов",
        "muted",
    ));
    let profile_message = widgets::label("Загрузка профилей…", "status-message");
    profile_message.set_wrap(true);
    images.append(&profile_message);
    let profile_title = widgets::label("", "space-title");
    images.append(&profile_title);
    let profile_info = widgets::label("", "muted");
    profile_info.set_wrap(true);
    images.append(&profile_info);
    let profile_build = widgets::button("Собрать образ", "system-run-symbolic", true);
    profile_build.set_halign(gtk4::Align::Start);
    profile_build.set_sensitive(false);
    images.append(&profile_build);
    let operation_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    operation_panel.add_css_class("operation-card");
    operation_panel.set_visible(false);
    let operation_heading = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let operation_spinner = gtk4::Spinner::new();
    operation_heading.append(&operation_spinner);
    let operation_title = widgets::label("", "section-title");
    operation_title.set_hexpand(true);
    operation_title.set_wrap(true);
    let operation_clock = widgets::label("", "muted");
    operation_heading.append(&operation_title);
    operation_heading.append(&operation_clock);
    operation_panel.append(&operation_heading);
    let operation_status = widgets::label("", "operation-status");
    operation_status.set_wrap(true);
    operation_status.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    operation_panel.append(&operation_status);
    operation_panel.append(&widgets::label(
        "Можно просматривать спейсы и состояние сети.",
        "muted",
    ));
    let operation_log = widgets::text_view();
    let operation_scroll = widgets::scroll(&operation_log);
    operation_scroll.set_min_content_height(220);
    operation_panel.append(&widgets::label(
        "Последние события операции",
        "section-title",
    ));
    operation_log.set_tooltip_text(Some(
        "До 200 последних строк; при перегрузке промежуточные сообщения могут пропускаться.",
    ));
    operation_panel.append(&operation_scroll);
    images.append(&operation_panel);
    let image_paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
    image_paned.set_position(270);
    image_paned.set_resize_start_child(false);
    image_paned.set_start_child(Some(&profile_sidebar));
    image_paned.set_end_child(Some(&widgets::scroll(&images)));
    pages.add_named(&image_paned, Some("images"));

    let net_page = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    net_page.add_css_class("content");
    net_page.append(&widgets::label("Сеть", "page-title"));
    net_page.append(&widgets::label("Наблюдения хоста · miyori-net", "muted"));
    let net_message = widgets::label("", "message-unavailable");
    net_message.set_wrap(true);
    net_message.set_visible(false);
    net_page.append(&net_message);
    let net_rows_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    net_rows_box.add_css_class("info-card");
    net_page.append(&net_rows_box);
    let net_note_label = widgets::label("", "warning");
    net_note_label.set_wrap(true);
    net_note_label.set_visible(false);
    net_page.append(&net_note_label);
    let ruleset_view = widgets::text_view();
    let ruleset_scroll = widgets::scroll(&ruleset_view);
    ruleset_scroll.set_min_content_height(240);
    let ruleset = gtk4::Expander::new(Some("Правила nftables"));
    ruleset.add_css_class("technical");
    ruleset.set_child(Some(&ruleset_scroll));
    net_page.append(&ruleset);
    pages.add_named(&widgets::scroll(&net_page), Some("network"));

    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 16);
    footer.add_css_class("status-bar");
    let connection_label = widgets::label("Подключение к демону…", "muted");
    let status_label = widgets::label("", "muted");
    status_label.set_hexpand(true);
    status_label.set_xalign(1.0);
    status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    footer.append(&connection_label);
    footer.append(&status_label);
    let activity = gtk4::Button::from_icon_name("view-list-symbolic");
    activity.set_tooltip_text(Some("Показать последнюю операцию"));
    footer.append(&activity);
    root.append(&footer);
    window.set_child(Some(&root));

    let ui = Rc::new(Ui {
        daemon,
        tx,
        next_id: Cell::new(0),
        window: window.clone(),
        status_label,
        connection_label,
        pages,
        navigation: navigation.clone(),
        search: search.clone(),
        all_filter: all,
        running_only: running_only.clone(),
        reconciling: Cell::new(false),
        space_widgets: RefCell::new(HashMap::new()),
        details,
        detail_header,
        selection_epoch: Cell::new(0),
        detail_request: RefCell::new(None),
        connected: Cell::new(false),
        closed: Cell::new(false),
        profiles: RefCell::new(Vec::new()),
        profile_list: profile_list.clone(),
        selected_profile: RefCell::new(None),
        profile_message,
        profile_title,
        profile_info,
        profile_build: profile_build.clone(),
        operation_panel,
        operation_title,
        operation_status,
        operation_clock,
        operation_spinner,
        operation_log,
        progress_lines: RefCell::new(VecDeque::new()),
        list_message,
        spaces_list: spaces_list.clone(),
        current_spaces: RefCell::new(Vec::new()),
        detail_message,
        log_view,
        net_message,
        net_rows_box,
        net_note_label,
        ruleset_view,
        create_button: create_button.clone(),
        start_button: start_button.clone(),
        stop_button: stop_button.clone(),
        open_window_button: open_window_button.clone(),
        build_button: build_button.clone(),
        update_image_button: update_image_button.clone(),
        reset_system_button: reset_system_button.clone(),
        reset_all_button: reset_all_button.clone(),
        destroy_button: destroy_button.clone(),
        selected: RefCell::new(None),
        pending_select: RefCell::new(None),
        latest_list: Cell::new(0),
        list_again: Cell::new(false),
        profiles_again: Cell::new(false),
        last_net: RefCell::new(None),
        latest_detail: Cell::new(0),
        latest_net: Cell::new(0),
        latest_profiles: Cell::new(0),
        pending_action: RefCell::new(None),
        create_dialog: RefCell::new(None),
    });
    {
        let weak = Rc::downgrade(&ui);
        spaces_list.connect_row_selected(move |_, row| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.reconciling.get() {
                return;
            }
            let id = row.map(|row| row.widget_name().to_string());
            select_space(&ui, id);
        });
    }
    for (index, button) in navigation.iter().enumerate() {
        let weak = Rc::downgrade(&ui);
        button.connect_toggled(move |button| {
            if !button.is_active() {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match index {
                0 => ui.show_detail_page(),
                1 => {
                    ui.show_images_page();
                    request_profiles_for_dialog(&ui);
                }
                _ => {
                    ui.show_net_page();
                    refresh_net(&ui);
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        search.connect_search_changed(move |_| {
            if let Some(ui) = weak.upgrade() {
                filter_spaces(&ui);
            }
        });
        let weak = Rc::downgrade(&ui);
        running_only.connect_toggled(move |_| {
            if let Some(ui) = weak.upgrade() {
                filter_spaces(&ui);
            }
        });
        let weak = Rc::downgrade(&ui);
        create_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                open_create_dialog(&ui);
            }
        });
        let weak = Rc::downgrade(&ui);
        activity.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.show_images_page();
                request_profiles_for_dialog(&ui);
            }
        });
        let weak = Rc::downgrade(&ui);
        profile_list.connect_row_selected(move |_, row| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.reconciling.get() {
                return;
            }
            *ui.selected_profile.borrow_mut() = row.map(|r| r.widget_name().to_string());
            update_profile_selection(&ui);
        });
        let weak = Rc::downgrade(&ui);
        profile_build.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let profile = ui.selected_profile.borrow().clone();
            if let Some(profile) = profile {
                start_build(&ui, profile);
            }
        });
    }
    for (button, op) in [
        (&start_button, "start"),
        (&stop_button, "stop"),
        (&open_window_button, "open-window"),
    ] {
        let weak = Rc::downgrade(&ui);
        button.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let selected = ui.selected.borrow().clone();
            if let Some(id) = selected {
                if needs_passphrase(op, &ui.current_spaces.borrow(), &id) {
                    open_passphrase_dialog(&ui, id.clone(), op, format!("{op} {id}"), false);
                } else {
                    send_action(
                        &ui,
                        serde_json::json!({"op":op,"space":id}),
                        format!("{op} {id}"),
                        PendingKind::Other,
                    );
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        build_button.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.show_images_page();
            let selected = ui.selected.borrow().clone();
            if let Some(profile) =
                selected.and_then(|id| profile_for_build(&ui.current_spaces.borrow(), &id))
            {
                *ui.selected_profile.borrow_mut() = Some(profile);
            }
            request_profiles_for_dialog(&ui);
        });
    }
    for (button, op) in [
        (&update_image_button, "update-image"),
        (&reset_system_button, "reset-system"),
        (&reset_all_button, "reset-all"),
        (&destroy_button, "destroy"),
    ] {
        let weak = Rc::downgrade(&ui);
        button.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let kind = match op {
                "update-image" => ConfirmKind::UpdateImage,
                "reset-system" => ConfirmKind::ResetSystem,
                "reset-all" => ConfirmKind::ResetAll,
                _ => ConfirmKind::Destroy,
            };
            confirm_and_send(&ui, kind, op);
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gtk4::gdk::Key::f && modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
            {
                ui.show_detail_page();
                ui.search.grab_focus();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
        let weak = Rc::downgrade(&ui);
        window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.closed.set(true);
                let dialog = ui.create_dialog.borrow().clone();
                if let Some(dialog) = dialog {
                    dialog.window.close();
                }
            }
            glib::Propagation::Proceed
        });
    }
    poll_updates(ui.clone(), rx);
    refresh_list(&ui);
    {
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_seconds_local(5, move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ui.closed.get() {
                return glib::ControlFlow::Break;
            }
            poll_refresh(&ui);
            glib::ControlFlow::Continue
        });
    }
    ui.sync_action_sensitivity();
    window.present();
    ui
}

// AlertDialog требует gtk4 feature v4_10 (не включена, это новая зависимость) — подтверждение обычным окном, без default-widget, чтобы Enter не бил по разрушительному действию
fn confirm_and_send(ui: &Rc<Ui>, kind: ConfirmKind, op: &'static str) {
    let Some(id) = ui.selected.borrow().clone() else {
        return;
    };
    let message = confirm_message(&kind, &id);

    let dialog = gtk4::Window::builder()
        .transient_for(&ui.window)
        .destroy_with_parent(true)
        .modal(true)
        .title("Подтверждение")
        .default_width(360)
        .build();
    apply_dialog_style(&dialog);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.add_css_class("dialog-content");
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);

    let message_label = gtk4::Label::new(Some(&message));
    message_label.add_css_class("dialog-message");
    message_label.set_wrap(true);
    message_label.set_xalign(0.0);
    content.append(&message_label);

    let buttons_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons_row.add_css_class("dialog-actions");
    buttons_row.set_halign(gtk4::Align::End);
    let cancel_button = gtk4::Button::with_label("Отмена");
    cancel_button.add_css_class("action-button");
    let confirm_button = gtk4::Button::with_label(match kind {
        ConfirmKind::Destroy => "Удалить спейс",
        ConfirmKind::ResetAll => "Сбросить всё",
        ConfirmKind::ResetSystem => "Сбросить систему",
        ConfirmKind::UpdateImage => "Обновить образ",
    });
    confirm_button.add_css_class("action-button");
    // подтверждение красится так же тревожно, как кнопка в списке; update-image, как и reset-system, — только предупреждением
    confirm_button.add_css_class(match kind {
        ConfirmKind::Destroy | ConfirmKind::ResetAll => "destructive-action",
        ConfirmKind::ResetSystem | ConfirmKind::UpdateImage => "action-caution",
    });
    buttons_row.append(&cancel_button);
    buttons_row.append(&confirm_button);
    content.append(&buttons_row);

    dialog.set_child(Some(&content));

    {
        let dialog = dialog.downgrade();
        cancel_button.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }
    {
        let ui = Rc::downgrade(ui);
        let dialog = dialog.downgrade();
        confirm_button.connect_clicked(move |_| {
            let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog.upgrade()) else {
                return;
            };
            dialog.close();
            // сперва «ты правда хочешь» (этот диалог), потом пароль — не одновременно
            if needs_passphrase(op, &ui.current_spaces.borrow(), &id) {
                open_passphrase_dialog(&ui, id.clone(), op, format!("{op} {id}"), false);
            } else {
                let request = serde_json::json!({"op": op, "space": id});
                send_action(&ui, request, format!("{op} {id}"), PendingKind::Other);
            }
        });
    }

    dialog.present();
}

// один маленький диалог обслуживает start/reset-system/reset-all/update-image — им всем нужен один и тот же пароль
fn open_passphrase_dialog(
    ui: &Rc<Ui>,
    id: String,
    op: &'static str,
    label: String,
    wrong_before: bool,
) {
    let dialog = gtk4::Window::builder()
        .transient_for(&ui.window)
        .destroy_with_parent(true)
        .modal(true)
        .title("Пароль спейса")
        .default_width(320)
        .build();
    apply_dialog_style(&dialog);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    content.add_css_class("dialog-content");
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);

    let message = gtk4::Label::new(Some(&format!("Спейс «{id}» зашифрован. Введите пароль.")));
    message.add_css_class("dialog-message");
    message.set_wrap(true);
    message.set_xalign(0.0);
    content.append(&message);

    if wrong_before {
        let wrong_label = gtk4::Label::new(Some("Пароль не подошёл, попробуйте ещё раз."));
        wrong_label.set_wrap(true);
        wrong_label.set_xalign(0.0);
        wrong_label.add_css_class("warning");
        content.append(&wrong_label);
    }

    let entry_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    entry_row.add_css_class("dialog-field-row");
    let entry_caption = gtk4::Label::new(Some("Пароль"));
    entry_caption.set_width_chars(8);
    entry_caption.set_xalign(0.0);
    entry_caption.add_css_class("dialog-field-caption");
    let entry = gtk4::Entry::new();
    entry.set_visibility(false);
    entry.add_css_class("dialog-entry");
    entry.set_hexpand(true);
    entry_row.append(&entry_caption);
    entry_row.append(&entry);
    content.append(&entry_row);

    let buttons_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons_row.add_css_class("dialog-actions");
    buttons_row.set_halign(gtk4::Align::End);
    let cancel_button = gtk4::Button::with_label("Отмена");
    cancel_button.add_css_class("action-button");
    let confirm_button = gtk4::Button::with_label("Подтвердить");
    confirm_button.add_css_class("action-button");
    confirm_button.add_css_class("suggested-action");
    buttons_row.append(&cancel_button);
    buttons_row.append(&confirm_button);
    content.append(&buttons_row);

    dialog.set_child(Some(&content));

    {
        let dialog = dialog.downgrade();
        cancel_button.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }
    {
        let ui = Rc::downgrade(ui);
        let dialog = dialog.downgrade();
        let entry = entry.downgrade();
        confirm_button.connect_clicked(move |_| {
            let (Some(ui), Some(dialog), Some(entry)) =
                (ui.upgrade(), dialog.upgrade(), entry.upgrade())
            else {
                return;
            };
            submit_passphrase(&ui, &entry, &dialog, &id, op, &label);
        });
    }

    {
        let entry = entry.downgrade();
        dialog.connect_close_request(move |_| {
            if let Some(entry) = entry.upgrade() {
                entry.set_text("");
            }
            glib::Propagation::Proceed
        });
    }
    dialog.present();
    entry.grab_focus();
}

// пароль читается из поля один раз и сразу стирается из него — дальше живёт только внутри request до отправки демону
fn submit_passphrase(
    ui: &Rc<Ui>,
    entry: &gtk4::Entry,
    dialog: &gtk4::Window,
    id: &str,
    op: &'static str,
    label: &str,
) {
    let passphrase = entry.text().to_string();
    entry.set_text("");
    if passphrase.is_empty() {
        return;
    }
    let request = serde_json::json!({"op": op, "space": id, "passphrase": passphrase});
    send_action(
        ui,
        request,
        label.to_string(),
        PendingKind::Passphrase {
            op,
            space: id.to_string(),
        },
    );
    dialog.close();
}

fn open_create_dialog(ui: &Rc<Ui>) {
    if let Some(dialog) = ui.create_dialog.borrow().as_ref() {
        dialog.window.present();
        return;
    }
    let dialog = gtk4::Window::builder()
        .transient_for(&ui.window)
        .destroy_with_parent(true)
        .modal(true)
        .title("Новый спейс")
        .default_width(620)
        .default_height(720)
        .build();
    apply_dialog_style(&dialog);
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.add_css_class("dialog-content");
    content.append(&widgets::label("Новый спейс", "page-title"));
    let intro = widgets::label("Отдельное окружение для вашего приложения", "muted");
    intro.set_wrap(true);
    content.append(&intro);
    let error_label = || {
        let label = widgets::label("", "warning");
        label.set_wrap(true);
        label.set_visible(false);
        label
    };
    let field = |caption: &str, widget: &gtk4::Widget| {
        let row = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let label = widgets::label(caption, "dialog-field-caption");
        label.set_mnemonic_widget(Some(widget));
        row.append(&label);
        widget.set_hexpand(true);
        row.append(widget);
        row
    };
    let preview = gtk4::Box::new(gtk4::Orientation::Horizontal, 14);
    preview.add_css_class("create-preview");
    let preview_bar = gtk4::DrawingArea::new();
    preview_bar.set_content_width(5);
    let preview_color = Rc::new(Cell::new(parse_hex_color(PRESET_COLORS[0])));
    let color = preview_color.clone();
    preview_bar.set_draw_func(move |_, cr, width, height| {
        let (r, g, b) = color.get();
        cr.set_source_rgb(r, g, b);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = cr.fill();
    });
    preview.append(&preview_bar);
    let icon = gtk4::Image::from_icon_name("application-x-executable-symbolic");
    icon.set_pixel_size(32);
    preview.append(&icon);
    let preview_text = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let preview_title = widgets::label("Ваш новый спейс", "section-title");
    let preview_subtitle = widgets::label("", "muted");
    preview_subtitle.set_wrap(true);
    preview_text.append(&preview_title);
    preview_text.append(&preview_subtitle);
    preview.append(&preview_text);
    content.append(&preview);

    let profile_dropdown = gtk4::DropDown::from_strings(&["Профили загружаются…"]);
    profile_dropdown.set_sensitive(false);
    content.append(&field("Профиль приложения", profile_dropdown.upcast_ref()));
    let profile_hint = error_label();
    content.append(&profile_hint);
    let profile_build_button = widgets::button("Собрать образ", "system-run-symbolic", false);
    profile_build_button.set_visible(false);
    profile_build_button.set_halign(gtk4::Align::Start);
    content.append(&profile_build_button);
    let status_label = widgets::label("", "muted");
    status_label.set_wrap(true);
    status_label.set_visible(false);
    content.append(&status_label);
    let id_entry = gtk4::Entry::builder()
        .placeholder_text("например, work-browser")
        .max_length(32)
        .build();
    let identity = gtk4::Box::new(gtk4::Orientation::Horizontal, 14);
    let id_field = field("Имя спейса", id_entry.upcast_ref());
    id_field.set_hexpand(true);
    identity.append(&id_field);
    let id_error_label = error_label();
    id_field.append(&id_error_label);
    let label_combo = gtk4::ComboBoxText::with_entry();
    for preset in PRESET_LABELS {
        label_combo.append_text(preset);
    }
    label_combo.set_active(Some(0));
    let label_field = field("Метка", label_combo.upcast_ref());
    label_field.set_hexpand(true);
    identity.append(&label_field);
    let label_error_label = error_label();
    label_field.append(&label_error_label);
    content.append(&identity);

    content.append(&widgets::label("Цвет окружения", "dialog-field-caption"));
    let colors = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let mut color_checks = Vec::new();
    let mut first: Option<gtk4::ToggleButton> = None;
    for hex in PRESET_COLORS {
        let check = gtk4::ToggleButton::new();
        check.add_css_class("swatch");
        check.set_tooltip_text(Some(hex));
        let swatch = gtk4::DrawingArea::new();
        swatch.set_content_width(20);
        swatch.set_content_height(20);
        let (r, g, b) = parse_hex_color(hex);
        swatch.set_draw_func(move |_, cr, width, height| {
            cr.set_source_rgb(r, g, b);
            cr.arc(
                width as f64 / 2.0,
                height as f64 / 2.0,
                9.0,
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
        });
        check.set_child(Some(&swatch));
        if let Some(first) = &first {
            check.set_group(Some(first));
        } else {
            first = Some(check.clone());
            check.set_active(true);
        }
        colors.append(&check);
        color_checks.push((hex.to_string(), check));
    }
    content.append(&colors);
    let color_manual_entry = gtk4::Entry::builder()
        .placeholder_text("#rrggbb")
        .max_length(7)
        .width_chars(8)
        .hexpand(true)
        .build();
    colors.append(&color_manual_entry);
    let color_error_label = error_label();
    content.append(&color_error_label);
    let seed_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let seed_entry = gtk4::Entry::builder()
        .placeholder_text("Необязательно · абсолютный путь")
        .hexpand(true)
        .build();
    seed_row.append(&seed_entry);
    let browse = widgets::button("Выбрать", "folder-open-symbolic", false);
    seed_row.append(&browse);
    content.append(&field("Начальные файлы для /data", seed_row.upcast_ref()));
    let seed_note = widgets::label("Каталог копируется только при создании спейса.", "muted");
    content.append(&seed_note);

    let encrypt_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let encrypt_label = widgets::label("Шифровать диск спейса", "section-title");
    encrypt_label.set_hexpand(true);
    encrypt_row.append(&encrypt_label);
    let encrypt = gtk4::Switch::new();
    encrypt.set_valign(gtk4::Align::Center);
    encrypt_row.append(&encrypt);
    content.append(&encrypt_row);
    let password_fields = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    let passphrase_entry = gtk4::Entry::builder()
        .placeholder_text("Пароль")
        .visibility(false)
        .build();
    let passphrase_confirm_entry = gtk4::Entry::builder()
        .placeholder_text("Повторите пароль")
        .visibility(false)
        .build();
    password_fields.append(&passphrase_entry);
    password_fields.append(&passphrase_confirm_entry);
    let passphrase_error_label = error_label();
    password_fields.append(&passphrase_error_label);
    let note = widgets::label(
        "Забытый пароль означает потерю данных спейса — восстановления нет.",
        "warning",
    );
    note.set_wrap(true);
    password_fields.append(&note);
    password_fields.set_visible(false);
    content.append(&password_fields);

    root.append(&widgets::scroll(&content));
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    actions.add_css_class("dialog-footer");
    actions.set_halign(gtk4::Align::End);
    let cancel = widgets::button("Отмена", "", false);
    let create_button = widgets::button("Создать спейс", "list-add-symbolic", true);
    actions.append(&cancel);
    actions.append(&create_button);
    root.append(&actions);
    dialog.set_child(Some(&root));
    let dlg = Rc::new(CreateDialog {
        window: dialog.clone(),
        seed_chooser: RefCell::new(None),
        encrypt: encrypt.clone(),
        password_fields,
        preview_title,
        preview_subtitle,
        preview_color,
        preview_bar,
        profiles: RefCell::new(Vec::new()),
        profile_dropdown: profile_dropdown.clone(),
        profile_hint,
        profile_build_button: profile_build_button.clone(),
        status_label,
        id_entry: id_entry.clone(),
        id_error_label,
        label_combo: label_combo.clone(),
        label_error_label,
        color_checks: color_checks.clone(),
        color_manual_entry: color_manual_entry.clone(),
        color_error_label,
        seed_entry: seed_entry.clone(),
        passphrase_entry: passphrase_entry.clone(),
        passphrase_confirm_entry: passphrase_confirm_entry.clone(),
        passphrase_error_label,
        create_button: create_button.clone(),
        can_create: Cell::new(false),
        can_build: Cell::new(false),
    });
    *ui.create_dialog.borrow_mut() = Some(dlg.clone());
    for entry in [
        &id_entry,
        &color_manual_entry,
        &passphrase_entry,
        &passphrase_confirm_entry,
    ] {
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        entry.connect_changed(move |_| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                revalidate(&ui, &dlg);
            }
        });
    }
    for (_, check) in &color_checks {
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        check.connect_toggled(move |check| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                if check.is_active() {
                    dlg.color_manual_entry.set_text("");
                }
                revalidate(&ui, &dlg);
            }
        });
    }
    {
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        label_combo.connect_changed(move |_| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                revalidate(&ui, &dlg);
            }
        });
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        profile_dropdown.connect_selected_notify(move |_| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                revalidate(&ui, &dlg);
            }
        });
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        encrypt.connect_active_notify(move |switch| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                dlg.password_fields.set_visible(switch.is_active());
                if !switch.is_active() {
                    dlg.passphrase_entry.set_text("");
                    dlg.passphrase_confirm_entry.set_text("");
                }
                revalidate(&ui, &dlg);
            }
        });
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        profile_build_button.connect_clicked(move |_| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                if let Some(profile) = selected_profile(&dlg) {
                    start_build(&ui, profile.profile);
                }
            }
        });
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        create_button.connect_clicked(move |_| {
            if let (Some(ui), Some(dlg)) = (weak.upgrade(), form.upgrade()) {
                try_submit_create(&ui, &dlg, &dlg.window);
            }
        });
        let window = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
        let weak = Rc::downgrade(ui);
        let form = Rc::downgrade(&dlg);
        dialog.connect_close_request(move |_| {
            if let Some(dlg) = form.upgrade() {
                dlg.passphrase_entry.set_text("");
                dlg.passphrase_confirm_entry.set_text("");
                let chooser = dlg.seed_chooser.borrow_mut().take();
                if let Some(chooser) = chooser {
                    chooser.destroy();
                }
            }
            if let Some(ui) = weak.upgrade() {
                ui.create_dialog.borrow_mut().take();
            }
            glib::Propagation::Proceed
        });
        let form = Rc::downgrade(&dlg);
        browse.connect_clicked(move |_| {
            let Some(dlg) = form.upgrade() else {
                return;
            };
            if let Some(chooser) = dlg.seed_chooser.borrow().as_ref() {
                chooser.show();
                return;
            }
            let chooser = gtk4::FileChooserNative::new(
                Some("Начальные файлы"),
                Some(&dlg.window),
                gtk4::FileChooserAction::SelectFolder,
                Some("Выбрать"),
                Some("Отмена"),
            );
            let form = Rc::downgrade(&dlg);
            chooser.connect_response(move |chooser, response| {
                if let Some(dlg) = form.upgrade() {
                    if response == gtk4::ResponseType::Accept {
                        if let Some(path) = chooser.file().and_then(|file| file.path()) {
                            dlg.seed_entry.set_text(&path.to_string_lossy());
                        }
                    }
                    dlg.seed_chooser.borrow_mut().take();
                }
                chooser.destroy();
            });
            *dlg.seed_chooser.borrow_mut() = Some(chooser.clone());
            chooser.show();
        });
        let window = dialog.downgrade();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dialog.add_controller(keys);
    }
    revalidate(ui, &dlg);
    dialog.present();
    request_profiles_for_dialog(ui);
}

// сброшенный манифест важнее отсутствия образа — про приоритет см. profile_eligibility
fn revalidate(ui: &Rc<Ui>, dlg: &Rc<CreateDialog>) {
    let profile = selected_profile(dlg);
    let (profile_ok, hint, build_visible) = match &profile {
        None => (false, Some("нет доступных профилей".to_string()), false),
        Some(row) => match profile_eligibility(row) {
            ProfileEligibility::Ready => (true, None, false),
            ProfileEligibility::NoTemplate => (
                false,
                Some("образ этого профиля ещё не собран.".to_string()),
                true,
            ),
            ProfileEligibility::ManifestBroken(err) => (
                false,
                Some(format!("манифест профиля не читается: {err}")),
                false,
            ),
        },
    };
    match hint {
        Some(text) => {
            dlg.profile_hint.set_label(&text);
            dlg.profile_hint.set_visible(true);
        }
        None => dlg.profile_hint.set_visible(false),
    }
    dlg.profile_build_button.set_visible(build_visible);
    dlg.can_build.set(build_visible);

    let existing_ids: Vec<String> = ui
        .current_spaces
        .borrow()
        .iter()
        .map(|space| space.id.clone())
        .collect();
    let id_problem = id_error(&dlg.id_entry.text(), &existing_ids);
    match &id_problem {
        Some(text) => {
            dlg.id_error_label.set_label(text);
            dlg.id_error_label.set_visible(true);
        }
        None => dlg.id_error_label.set_visible(false),
    }

    let label_text = dlg
        .label_combo
        .active_text()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let label_problem = slug_error(&label_text);
    match &label_problem {
        Some(text) => {
            dlg.label_error_label.set_label(text);
            dlg.label_error_label.set_visible(true);
        }
        None => dlg.label_error_label.set_visible(false),
    }

    let color_problem = color_error(&effective_color(dlg));
    match &color_problem {
        Some(text) => {
            dlg.color_error_label.set_label(text);
            dlg.color_error_label.set_visible(true);
        }
        None => dlg.color_error_label.set_visible(false),
    }

    let passphrase_problem = if dlg.encrypt.is_active() {
        if dlg.passphrase_entry.text().is_empty() {
            Some("Введите пароль для шифрования.".to_string())
        } else {
            passphrase_error(
                &dlg.passphrase_entry.text(),
                &dlg.passphrase_confirm_entry.text(),
            )
        }
    } else {
        None
    };
    let preview_id = dlg.id_entry.text();
    dlg.preview_title.set_text(if preview_id.is_empty() {
        "Ваш новый спейс"
    } else {
        &preview_id
    });
    dlg.preview_subtitle.set_text(&format!(
        "{} · метка {} · {}",
        profile
            .as_ref()
            .map(|p| p.profile.as_str())
            .unwrap_or("Выберите профиль"),
        label_text,
        if dlg.encrypt.is_active() {
            "С шифрованием"
        } else {
            "Без шифрования"
        }
    ));
    dlg.preview_color
        .set(parse_hex_color(&effective_color(dlg)));
    dlg.preview_bar.queue_draw();
    match &passphrase_problem {
        Some(text) => {
            dlg.passphrase_error_label.set_label(text);
            dlg.passphrase_error_label.set_visible(true);
        }
        None => dlg.passphrase_error_label.set_visible(false),
    }

    dlg.can_create.set(
        profile_ok
            && id_problem.is_none()
            && label_problem.is_none()
            && color_problem.is_none()
            && passphrase_problem.is_none(),
    );
    ui.sync_action_sensitivity();
}

fn try_submit_create(ui: &Rc<Ui>, dlg: &Rc<CreateDialog>, dialog: &gtk4::Window) {
    revalidate(ui, dlg);
    if !ui.connected.get() || !dlg.can_create.get() || ui.pending_action.borrow().is_some() {
        return;
    }
    let Some(profile) = selected_profile(dlg) else {
        return;
    };
    let id = dlg.id_entry.text().to_string();
    let label = dlg
        .label_combo
        .active_text()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let color = effective_color(dlg);
    let mut request = serde_json::json!({
        "op": "create",
        "space": id,
        "profile": profile.profile,
        "label": label,
        "color": color,
    });
    // пусто — как раньше, seed в запрос вообще не попадает
    let seed = dlg.seed_entry.text().trim().to_string();
    if !seed.is_empty() {
        request["seed"] = serde_json::json!(seed);
    }
    // Поля очищаются до отправки, чтобы закрытая форма не сохраняла пароль.
    let passphrase = if dlg.encrypt.is_active() {
        dlg.passphrase_entry.text().to_string()
    } else {
        String::new()
    };
    let encrypted = dlg.encrypt.is_active();
    dlg.passphrase_entry.set_text("");
    dlg.passphrase_confirm_entry.set_text("");
    if !passphrase.is_empty() {
        request["passphrase"] = serde_json::json!(passphrase);
    }
    send_action(
        ui,
        request,
        format!("создание спейса {id}"),
        PendingKind::Create {
            new_id: id,
            encrypted,
        },
    );
    dialog.close();
}

fn request_profiles_for_dialog(ui: &Rc<Ui>) {
    if ui.closed.get() || ui.latest_profiles.get() != 0 {
        return;
    }
    let id = ui.next_request_id();
    ui.latest_profiles.set(id);
    ui.daemon
        .send(id, serde_json::json!({"op": "profiles"}), ui.tx.clone());
}

fn render_profiles(ui: &Rc<Ui>, view: view::ProfilesView) {
    let (rows, error) = match view {
        view::ProfilesView::Profiles(rows) => (rows, None),
        view::ProfilesView::Unavailable { message } => (Vec::new(), Some(message)),
    };
    let previous = ui.selected_profile.borrow().clone();
    let selected = previous
        .filter(|id| rows.iter().any(|p| &p.profile == id))
        .or_else(|| rows.first().map(|p| p.profile.clone()));
    if *ui.profiles.borrow() != rows {
        ui.reconciling.set(true);
        while let Some(child) = ui.profile_list.first_child() {
            ui.profile_list.remove(&child);
        }
        for profile in &rows {
            let row = gtk4::ListBoxRow::new();
            row.set_widget_name(&profile.profile);
            row.add_css_class("space-row");
            let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            content.append(&gtk4::Image::from_icon_name(widgets::space_icon(
                &profile.profile,
            )));
            let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            texts.append(&widgets::label(&profile.profile, "space-name"));
            let state = if !profile.manifest_ok {
                "Ошибка манифеста"
            } else if profile.template {
                "Образ готов"
            } else {
                "Нужна сборка"
            };
            texts.append(&widgets::label(state, "space-subtitle"));
            content.append(&texts);
            row.set_child(Some(&content));
            ui.profile_list.append(&row);
        }
        ui.profiles.replace(rows.clone());
        ui.reconciling.set(false);
    }
    *ui.selected_profile.borrow_mut() = selected.clone();
    ui.reconciling.set(true);
    let index = rows
        .iter()
        .position(|p| Some(&p.profile) == selected.as_ref());
    let row = index.and_then(|i| ui.profile_list.row_at_index(i as i32));
    ui.profile_list.select_row(row.as_ref());
    ui.reconciling.set(false);
    ui.profile_message
        .set_text(error.as_deref().unwrap_or(if rows.is_empty() {
            "Профилей пока нет. Добавьте профиль в каталог профилей демона."
        } else {
            ""
        }));
    ui.profile_message
        .set_visible(error.is_some() || rows.is_empty());
    update_profile_selection(ui);
    let dialog = ui.create_dialog.borrow().clone();
    if let Some(dlg) = dialog {
        let previous = selected_profile(&dlg).map(|p| p.profile);
        if *dlg.profiles.borrow() != rows {
            let index = rows
                .iter()
                .position(|p| Some(&p.profile) == previous.as_ref())
                .unwrap_or(0);
            let texts: Vec<String> = rows.iter().map(profile_option_text).collect();
            dlg.profiles.replace(rows);
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            dlg.profile_dropdown
                .set_model(Some(&gtk4::StringList::new(&refs)));
            dlg.profile_dropdown.set_selected(index as u32);
        }
        dlg.profile_dropdown
            .set_sensitive(!dlg.profiles.borrow().is_empty());
        revalidate(ui, &dlg);
        if let Some(error) = error {
            dlg.profile_hint
                .set_text(&format!("Профили недоступны: {error}"));
            dlg.profile_hint.set_visible(true);
        }
    }
}

fn update_profile_selection(ui: &Rc<Ui>) {
    let selected = ui.selected_profile.borrow().clone();
    let profile = selected.and_then(|id| {
        ui.profiles
            .borrow()
            .iter()
            .find(|p| p.profile == id)
            .cloned()
    });
    if let Some(profile) = profile {
        ui.profile_title.set_text(&profile.profile);
        ui.profile_info
            .set_text(&match profile_eligibility(&profile) {
                ProfileEligibility::Ready => format!(
                    "Образ готов · уровень {}\nНовая сборка не меняет уже созданные спейсы.",
                    profile.isolation_level.as_deref().unwrap_or("—")
                ),
                ProfileEligibility::NoTemplate => format!(
                    "Образ ещё не собран · уровень {}\nСоберите его перед созданием спейса.",
                    profile.isolation_level.as_deref().unwrap_or("—")
                ),
                ProfileEligibility::ManifestBroken(error) => {
                    format!("Манифест не читается: {error}")
                }
            });
    } else {
        ui.profile_title.set_text("");
        ui.profile_info.set_text("");
    }
    ui.sync_action_sensitivity();
}

fn prompt_start_after_create(ui: &Rc<Ui>, id: String, encrypted: bool) {
    let dialog = gtk4::Window::builder()
        .transient_for(&ui.window)
        .destroy_with_parent(true)
        .modal(true)
        .title("Спейс создан")
        .default_width(320)
        .build();

    apply_dialog_style(&dialog);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.add_css_class("dialog-content");
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);

    let message = gtk4::Label::new(Some(&format!("Спейс «{id}» создан. Запустить сейчас?")));
    message.set_wrap(true);
    message.set_xalign(0.0);
    content.append(&message);

    let buttons_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons_row.set_halign(gtk4::Align::End);
    let later_button = gtk4::Button::with_label("Позже");
    let start_now_button = gtk4::Button::with_label("Запустить");
    buttons_row.append(&later_button);
    buttons_row.append(&start_now_button);
    content.append(&buttons_row);

    dialog.set_child(Some(&content));

    {
        let dialog = dialog.downgrade();
        later_button.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }
    {
        let ui = Rc::downgrade(ui);
        let dialog = dialog.downgrade();
        start_now_button.connect_clicked(move |_| {
            let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog.upgrade()) else {
                return;
            };
            if encrypted {
                open_passphrase_dialog(&ui, id.clone(), "start", format!("Запуск {id}"), false);
            } else {
                send_action(
                    &ui,
                    serde_json::json!({"op":"start","space":id}),
                    format!("Запуск {id}"),
                    PendingKind::Other,
                );
            }
            dialog.close();
        });
    }

    dialog.present();
}

fn start_build(ui: &Rc<Ui>, profile: String) {
    let Some(profile) = normalize_profile_input(&profile) else {
        return;
    };
    send_action(
        ui,
        serde_json::json!({"op":"build","profile":profile}),
        format!("Сборка образа {profile}"),
        PendingKind::Other,
    );
}

fn send_action(ui: &Rc<Ui>, request: serde_json::Value, label: String, kind: PendingKind) {
    if ui.closed.get() || !ui.connected.get() || ui.pending_action.borrow().is_some() {
        return;
    }
    let id = ui.next_request_id();
    *ui.pending_action.borrow_mut() = Some(PendingAction {
        id,
        label: label.clone(),
        kind,
        started: Instant::now(),
    });
    ui.progress_lines.borrow_mut().clear();
    set_buffer_text(&ui.operation_log, "");
    ui.operation_panel.set_visible(true);
    ui.operation_spinner.start();
    ui.operation_title.set_text(&label);
    ui.operation_status.remove_css_class("status-error");
    ui.status_label.remove_css_class("status-error");
    ui.operation_status.set_text("Выполняется…");
    ui.operation_clock.set_text("0:00");
    ui.status_label.set_text(&format!("{label}…"));
    ui.status_label.set_tooltip_text(Some(&label));
    ui.sync_action_sensitivity();
    ui.daemon.send(id, request, ui.tx.clone());
}

fn refresh_list(ui: &Rc<Ui>) {
    if ui.closed.get() || ui.latest_list.get() != 0 {
        return;
    }
    let id = ui.next_request_id();
    ui.latest_list.set(id);
    ui.daemon
        .send(id, serde_json::json!({"op":"list"}), ui.tx.clone());
}

fn refresh_detail(ui: &Rc<Ui>, space: String) {
    if ui.closed.get() || !ui.connected.get() || ui.latest_detail.get() != 0 {
        return;
    }
    let id = ui.next_request_id();
    ui.latest_detail.set(id);
    *ui.detail_request.borrow_mut() = Some((space.clone(), ui.selection_epoch.get()));
    ui.daemon.send(
        id,
        serde_json::json!({"op":"describe","space":space}),
        ui.tx.clone(),
    );
}

fn refresh_net(ui: &Rc<Ui>) {
    if ui.closed.get() || ui.latest_net.get() != 0 {
        return;
    }
    let id = ui.next_request_id();
    ui.latest_net.set(id);
    ui.daemon
        .send(id, serde_json::json!({"op":"net-status"}), ui.tx.clone());
}

fn poll_refresh(ui: &Rc<Ui>) {
    refresh_list(ui);
    match ui.pages.visible_child_name().as_deref() {
        Some("network") => refresh_net(ui),
        Some("images") => request_profiles_for_dialog(ui),
        _ => {
            let selected = ui.selected.borrow().clone();
            if let Some(id) = selected {
                refresh_detail(ui, id);
            }
        }
    }
    if ui.create_dialog.borrow().is_some() {
        request_profiles_for_dialog(ui);
    }
}

fn append_progress(ui: &Ui, text: &str) {
    let mut lines = ui.progress_lines.borrow_mut();
    for line in text.lines() {
        lines.push_back(line.chars().take(1024).collect());
        if lines.len() > 200 {
            lines.pop_front();
        }
    }
}

fn render_progress(ui: &Ui) {
    let lines = ui.progress_lines.borrow();
    set_buffer_text(
        &ui.operation_log,
        &lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

fn handle_progress(ui: &Ui, request_id: u64, text: &str) {
    if !ui
        .pending_action
        .borrow()
        .as_ref()
        .is_some_and(|action| action.id == request_id)
    {
        return;
    }
    append_progress(ui, text);
    ui.operation_status.set_text(text);
    ui.status_label.set_text(text);
    ui.status_label.set_tooltip_text(Some(text));
    if let Some(dialog) = ui.create_dialog.borrow().as_ref() {
        dialog.status_label.set_text(text);
        dialog.status_label.set_visible(true);
    }
}

fn poll_updates(ui: Rc<Ui>, rx: Receiver<Update>) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if ui.closed.get() {
            return glib::ControlFlow::Break;
        }
        let mut changed = false;
        for _ in 0..64 {
            let Ok(update) = rx.try_recv() else {
                break;
            };
            match update {
                Update::Progress { request_id, text } => {
                    handle_progress(&ui, request_id, &text);
                    changed = true;
                }
                Update::Done { request_id, reply } => handle_done(&ui, request_id, reply),
            }
        }
        if changed {
            render_progress(&ui);
        }
        if let Some(action) = ui.pending_action.borrow().as_ref() {
            let secs = action.started.elapsed().as_secs();
            ui.operation_clock
                .set_text(&format!("{}:{:02}", secs / 60, secs % 60));
        }
        glib::ControlFlow::Continue
    });
}

fn handle_done(ui: &Rc<Ui>, request_id: u64, reply: Result<Reply, String>) {
    if request_id == 0 || ui.closed.get() {
        return;
    }
    if request_id == ui.latest_list.get() {
        ui.latest_list.set(0);
        if ui.list_again.replace(false) {
            refresh_list(ui);
        } else {
            render_spaces(ui, view::spaces_view(reply));
        }
        return;
    }
    if request_id == ui.latest_detail.get() {
        ui.latest_detail.set(0);
        let request = ui.detail_request.borrow_mut().take();
        let selected = ui.selected.borrow().clone();
        let current = request.as_ref().is_some_and(|(id, epoch)| {
            selected.as_ref() == Some(id) && *epoch == ui.selection_epoch.get()
        });
        if current && ui.connected.get() {
            render_detail(ui, view::detail_view(reply));
        } else if ui.connected.get() {
            if let Some(id) = selected {
                refresh_detail(ui, id);
            }
        }
        return;
    }
    if request_id == ui.latest_net.get() {
        ui.latest_net.set(0);
        render_net(ui, view::net_view(reply));
        return;
    }
    if request_id == ui.latest_profiles.get() {
        ui.latest_profiles.set(0);
        if ui.profiles_again.replace(false) {
            request_profiles_for_dialog(ui);
        } else {
            render_profiles(ui, view::profiles_view(reply));
        }
        return;
    }
    let taken = {
        let mut pending = ui.pending_action.borrow_mut();
        if pending
            .as_ref()
            .is_some_and(|action| action.id == request_id)
        {
            pending.take()
        } else {
            None
        }
    };
    let Some(action) = taken else {
        return;
    };
    ui.operation_spinner.stop();
    let success = matches!(reply, Ok(Reply::Ok(_)));
    let message = match &reply {
        Err(transport) => transport.clone(),
        Ok(Reply::Err { code, message }) => format!("{code}: {message}"),
        Ok(Reply::Ok(_)) => format!("{}: выполнено", action.label),
    };
    ui.status_label.set_text(&message);
    ui.status_label.set_tooltip_text(Some(&message));
    ui.operation_status.set_text(&message);
    if !success {
        ui.operation_status.add_css_class("status-error");
        ui.status_label.add_css_class("status-error");
    }
    append_progress(ui, &message);
    render_progress(ui);
    if let Some(dialog) = ui.create_dialog.borrow().as_ref() {
        dialog.status_label.set_text(&message);
        dialog.status_label.set_visible(true);
    }
    ui.sync_action_sensitivity();
    if let Ok(Reply::Err { code, .. }) = &reply {
        if code == "wrong-passphrase" {
            if let PendingKind::Passphrase { op, space } = action.kind {
                open_passphrase_dialog(ui, space, op, action.label, true);
                return;
            }
        }
    }
    if success {
        if let PendingKind::Create { new_id, encrypted } = action.kind {
            *ui.pending_select.borrow_mut() = Some(new_id.clone());
            prompt_start_after_create(ui, new_id, encrypted);
        }
        ui.selection_epoch.set(ui.selection_epoch.get() + 1);
        if ui.latest_list.get() != 0 {
            ui.list_again.set(true);
        } else {
            refresh_list(ui);
        }
        let selected = ui.selected.borrow().clone();
        if let Some(id) = selected {
            refresh_detail(ui, id);
        }
        if ui.latest_profiles.get() != 0 {
            ui.profiles_again.set(true);
        } else {
            request_profiles_for_dialog(ui);
        }
    }
}

fn clear_box_children(container: &gtk4::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

// опрос перерисовывает журнал каждые 5 с: переписать буфер тем же текстом — сбросить скролл там, где оператор читает
fn set_buffer_text(view: &gtk4::TextView, text: &str) {
    let buffer = view.buffer();
    let current = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    if current != text {
        buffer.set_text(text);
    }
}

// «спейс не выбран» / «спейсов пока нет» — не ошибка, крупный центрированный текст (документации проекта §2)
fn mark_detail_message_empty(label: &gtk4::Label) {
    label.set_halign(gtk4::Align::Center);
    label.set_valign(gtk4::Align::Center);
    label.set_vexpand(true);
    label.set_justify(gtk4::Justification::Center);
    label.add_css_class("empty-state");
}

// отказ демона не должен наследовать центрирование пустого состояния — иначе спокойный вид перекроет ошибку
fn mark_detail_message_error(label: &gtk4::Label) {
    label.set_halign(gtk4::Align::Fill);
    label.set_valign(gtk4::Align::Start);
    label.set_vexpand(false);
    label.remove_css_class("empty-state");
}

fn render_spaces(ui: &Rc<Ui>, view: view::SpacesView) {
    match view {
        view::SpacesView::Unavailable { message } => {
            ui.connected.set(false);
            ui.selection_epoch.set(ui.selection_epoch.get() + 1);
            ui.connection_label.set_text("Демон недоступен");
            ui.connection_label.add_css_class("status-error");
            ui.reconciling.set(true);
            for widgets in ui.space_widgets.borrow().values() {
                ui.spaces_list.remove(&widgets.row);
            }
            ui.space_widgets.borrow_mut().clear();
            ui.current_spaces.borrow_mut().clear();
            ui.reconciling.set(false);
            select_space(ui, None);
            ui.list_message.set_text(&message);
            ui.list_message.add_css_class("message-unavailable");
            ui.list_message.set_visible(true);
            ui.detail_message
                .set_text("Спейсы недоступны. Ожидаем подключения к демону.");
            mark_detail_message_error(&ui.detail_message);
        }
        view::SpacesView::Spaces(rows) => {
            ui.connected.set(true);
            ui.connection_label.set_text("Демон подключён");
            ui.connection_label.remove_css_class("status-error");
            ui.list_message.remove_css_class("message-unavailable");
            ui.reconciling.set(true);
            {
                let ids: HashSet<&String> = rows.iter().map(|row| &row.id).collect();
                let mut widgets = ui.space_widgets.borrow_mut();
                widgets.retain(|id, widgets| {
                    let keep = ids.contains(id);
                    if !keep {
                        ui.spaces_list.remove(&widgets.row);
                    }
                    keep
                });
                for (index, row) in rows.iter().enumerate() {
                    let item = widgets.entry(row.id.clone()).or_insert_with(|| {
                        let list_row = gtk4::ListBoxRow::new();
                        list_row.set_widget_name(&row.id);
                        list_row.add_css_class("space-row");
                        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
                        let bar = gtk4::DrawingArea::new();
                        bar.set_content_width(4);
                        bar.add_css_class("space-color-bar");
                        let color = Rc::new(Cell::new(parse_hex_color(&row.color)));
                        let draw_color = color.clone();
                        bar.set_draw_func(move |_, cr, width, height| {
                            let (r, g, b) = draw_color.get();
                            cr.set_source_rgb(r, g, b);
                            cr.rectangle(0.0, 0.0, width as f64, height as f64);
                            let _ = cr.fill();
                        });
                        content.append(&bar);
                        let icon = gtk4::Image::from_icon_name(widgets::space_icon(&row.profile));
                        icon.set_pixel_size(24);
                        content.append(&icon);
                        let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 5);
                        texts.set_hexpand(true);
                        let title = widgets::label(&row.id, "space-name");
                        let subtitle = widgets::label("", "space-subtitle");
                        for label in [&title, &subtitle] {
                            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                            label.set_max_width_chars(24);
                            texts.append(label);
                        }
                        content.append(&texts);
                        list_row.set_child(Some(&content));
                        ui.spaces_list.insert(&list_row, index as i32);
                        SpaceWidgets {
                            row: list_row,
                            title,
                            subtitle,
                            color,
                            bar,
                            icon,
                        }
                    });
                    if item.row.index() != index as i32 {
                        ui.spaces_list.remove(&item.row);
                        ui.spaces_list.insert(&item.row, index as i32);
                    }
                    item.title.set_text(&row.id);
                    item.subtitle.set_text(&format!(
                        "{} · {}",
                        widgets::state_text(&row.state),
                        row.level
                    ));
                    item.icon
                        .set_icon_name(Some(widgets::space_icon(&row.profile)));
                    let color = parse_hex_color(&row.color);
                    if item.color.get() != color {
                        item.color.set(color);
                        item.bar.queue_draw();
                    }
                    for class in [
                        "state-running",
                        "state-stopped",
                        "state-unresponsive",
                        "state-unknown",
                        "level-reduced",
                    ] {
                        item.row.remove_css_class(class);
                    }
                    item.row.add_css_class(state_css_class(&row.state));
                    if row.level == "reduced" {
                        item.row.add_css_class("level-reduced");
                    }
                    item.row.set_tooltip_text(Some(&format!(
                        "{}\nМетка: {}\n{}",
                        space_row_text(row),
                        row.label,
                        row.level_reason
                    )));
                }
            }
            ui.current_spaces.replace(rows);
            let pending = ui.pending_select.borrow().clone();
            if let Some(id) =
                pending.filter(|id| ui.current_spaces.borrow().iter().any(|row| &row.id == id))
            {
                ui.pending_select.borrow_mut().take();
                ui.search.set_text("");
                ui.all_filter.set_active(true);
                *ui.selected.borrow_mut() = Some(id);
                ui.selection_epoch.set(ui.selection_epoch.get() + 1);
                ui.details.root.set_visible(false);
                set_buffer_text(&ui.log_view, "");
            }
            ui.reconciling.set(false);
            filter_spaces(ui);
        }
    }
    ui.sync_action_sensitivity();
}

fn filter_spaces(ui: &Rc<Ui>) {
    if ui.reconciling.get() {
        return;
    }
    let previous = ui.selected.borrow().clone();
    let query = ui.search.text().to_lowercase();
    let only_running = ui.running_only.is_active();
    ui.reconciling.set(true);
    let visible: Vec<String> = ui
        .current_spaces
        .borrow()
        .iter()
        .filter_map(|space| {
            let visible = (!only_running || space.state == "running")
                && format!("{} {} {}", space.id, space.profile, space.label)
                    .to_lowercase()
                    .contains(&query);
            if let Some(widget) = ui.space_widgets.borrow().get(&space.id) {
                widget.row.set_visible(visible);
            }
            visible.then(|| space.id.clone())
        })
        .collect();
    let target = previous
        .filter(|id| visible.contains(id))
        .or_else(|| visible.first().cloned());
    let row = target
        .as_ref()
        .and_then(|id| ui.space_widgets.borrow().get(id).map(|w| w.row.clone()));
    ui.spaces_list.select_row(row.as_ref());
    ui.reconciling.set(false);
    select_space(ui, target);
    if ui.connected.get() {
        ui.list_message.set_visible(visible.is_empty());
        ui.list_message
            .set_text(if ui.current_spaces.borrow().is_empty() {
                "Спейсов пока нет. Создайте первый."
            } else {
                "Ничего не найдено."
            });
    }
}

fn select_space(ui: &Rc<Ui>, selected: Option<String>) {
    let changed = *ui.selected.borrow() != selected;
    if changed {
        ui.selection_epoch.set(ui.selection_epoch.get() + 1);
        *ui.selected.borrow_mut() = selected.clone();
        ui.details.root.set_visible(false);
        ui.details.stack.set_visible_child_name("overview");
        set_buffer_text(&ui.log_view, "");
    }
    let row = selected.as_ref().and_then(|id| {
        ui.current_spaces
            .borrow()
            .iter()
            .find(|row| &row.id == id)
            .cloned()
    });
    if let Some(row) = row {
        ui.detail_header.set_visible(true);
        ui.details.title.set_text(&row.id);
        ui.details
            .subtitle
            .set_text(&format!("Профиль {}", row.profile));
        ui.details
            .icon
            .set_icon_name(Some(widgets::space_icon(&row.profile)));
        ui.details.state.set_text(widgets::state_text(&row.state));
        for class in [
            "state-running",
            "state-stopped",
            "state-unresponsive",
            "state-unknown",
        ] {
            ui.details.state.remove_css_class(class);
        }
        ui.details.state.add_css_class(state_css_class(&row.state));
        ui.details.level.set_text(&row.level);
        if row.level == "reduced" {
            ui.details.level.add_css_class("level-reduced");
        } else {
            ui.details.level.remove_css_class("level-reduced");
        }
        ui.details.level.set_tooltip_text(Some(&row.level_reason));
        ui.details
            .identity
            .set_text(&format!("Метка: {}", row.label));
        if changed || !ui.details.root.is_visible() {
            ui.detail_message.set_text("Загрузка сведений…");
            ui.detail_message.set_visible(true);
            mark_detail_message_error(&ui.detail_message);
            refresh_detail(ui, row.id);
        }
    } else {
        ui.detail_header.set_visible(false);
        ui.details.root.set_visible(false);
        ui.detail_message.set_visible(true);
        ui.detail_message
            .set_text(if ui.current_spaces.borrow().is_empty() {
                "Спейсов пока нет. Нажмите «Создать спейс»."
            } else {
                "Нет спейсов, подходящих под фильтр."
            });
        mark_detail_message_empty(&ui.detail_message);
    }
    ui.sync_action_sensitivity();
}

fn render_detail(ui: &Rc<Ui>, view: view::DetailView) {
    match view {
        view::DetailView::Unavailable { message } => {
            ui.detail_message.set_text(&message);
            mark_detail_message_error(&ui.detail_message);
            ui.detail_message.set_visible(true);
            ui.details.root.set_visible(false);
            set_buffer_text(&ui.log_view, "");
        }
        view::DetailView::Space { rows, log_tail } => {
            ui.detail_message.set_visible(false);
            ui.details.update(&rows);
            ui.details.root.set_visible(true);
            set_buffer_text(&ui.log_view, &log_tail.join("\n"));
        }
    }
}

fn render_net(ui: &Rc<Ui>, result: Result<view::NetView, String>) {
    if ui.last_net.borrow().as_ref() == Some(&result) {
        return;
    }
    ui.last_net.replace(Some(result.clone()));
    clear_box_children(&ui.net_rows_box);
    match result {
        Err(message) => {
            ui.net_message.set_label(&message);
            ui.net_message.set_visible(true);
            ui.net_note_label.set_visible(false);
            set_buffer_text(&ui.ruleset_view, "");
        }
        Ok(net) => {
            ui.net_message.set_visible(false);
            for row in net.rows {
                let label = gtk4::Label::new(Some(&format!("{}: {}", row.key, row.value)));
                label.set_xalign(0.0);
                label.set_wrap(true);
                label.set_selectable(true);
                label.add_css_class("detail-row-label");
                ui.net_rows_box.append(&label);
            }
            if net.ruleset_note.is_empty() {
                ui.net_note_label.set_visible(false);
            } else {
                ui.net_note_label
                    .set_label(&format!("⚠ {}", net.ruleset_note));
                ui.net_note_label.set_visible(true);
            }
            set_buffer_text(&ui.ruleset_view, &net.ruleset_text);
        }
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_color_valid_uses_all_three_channels() {
        let (r, g, b) = parse_hex_color("#3390ec");
        assert!((r - 0x33 as f64 / 255.0).abs() < f64::EPSILON);
        assert!((g - 0x90 as f64 / 255.0).abs() < f64::EPSILON);
        assert!((b - 0xec as f64 / 255.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_hex_color_missing_hash_falls_back_to_gray() {
        assert_eq!(parse_hex_color("3390ec"), (0.5, 0.5, 0.5));
    }

    #[test]
    fn parse_hex_color_wrong_length_falls_back_to_gray() {
        assert_eq!(parse_hex_color("#333"), (0.5, 0.5, 0.5));
    }

    #[test]
    fn parse_hex_color_non_hex_falls_back_to_gray() {
        assert_eq!(parse_hex_color("#zzzzzz"), (0.5, 0.5, 0.5));
    }

    #[test]
    fn parse_hex_color_multibyte_does_not_panic() {
        assert_eq!(parse_hex_color("#日本語ab"), (0.5, 0.5, 0.5));
    }

    // причина уровня спейса — реальный текст из profiles/spike-reduced/manifest.toml (стенд, задача 77)
    #[test]
    fn space_row_text_is_one_line_and_never_includes_level_reason() {
        let row = view::SpaceRow {
            id: "spike-reduced".to_string(),
            label: "spike-reduced".to_string(),
            color: "#557c94".to_string(),
            profile: "spike-reduced".to_string(),
            state: "stopped".to_string(),
            level: "reduced".to_string(),
            level_reason: "профиль стенда: уровень заявлен, чтобы его видимость можно было \
                проверить на живом спейсе"
                .to_string(),
            encrypted: false,
        };
        let text = space_row_text(&row);
        assert!(text.contains("spike-reduced"));
        assert!(text.contains("stopped"));
        assert!(text.contains("reduced"));
        assert!(!text.contains("профиль стенда"), "{text}");
        assert!(
            !text.contains('\n'),
            "строка списка должна быть одной строкой: {text:?}"
        );
    }

    #[test]
    fn space_row_text_omits_reason_when_standard() {
        let row = view::SpaceRow {
            id: "telegram".to_string(),
            label: "Telegram".to_string(),
            color: "#3390ec".to_string(),
            profile: "telegram".to_string(),
            state: "running".to_string(),
            level: "standard".to_string(),
            level_reason: String::new(),
            encrypted: false,
        };
        let text = space_row_text(&row);
        assert!(!text.contains(" — "));
    }

    #[test]
    fn state_css_class_running() {
        assert_eq!(state_css_class("running"), "state-running");
    }

    #[test]
    fn state_css_class_stopped() {
        assert_eq!(state_css_class("stopped"), "state-stopped");
    }

    #[test]
    fn state_css_class_unresponsive() {
        assert_eq!(state_css_class("unresponsive"), "state-unresponsive");
    }

    #[test]
    fn start_is_offered_only_to_a_stopped_space() {
        assert!(action_enabled(Action::Start, Some("stopped")));
        assert!(!action_enabled(Action::Start, Some("running")));
        assert!(!action_enabled(Action::Start, Some("unresponsive")));
    }

    // unresponsive — это живой QEMU, чей агент молчит: остановить его можно и нужно
    #[test]
    fn stop_is_offered_to_running_and_unresponsive() {
        assert!(action_enabled(Action::Stop, Some("running")));
        assert!(action_enabled(Action::Stop, Some("unresponsive")));
        assert!(!action_enabled(Action::Stop, Some("stopped")));
    }

    // у остановленного спейса демон гарантированно откажет open-window — кнопка не должна давать эту ловушку
    #[test]
    fn open_window_is_offered_only_to_a_running_space() {
        assert!(action_enabled(Action::OpenWindow, Some("running")));
        assert!(!action_enabled(Action::OpenWindow, Some("stopped")));
        assert!(!action_enabled(Action::OpenWindow, Some("unresponsive")));
    }

    #[test]
    fn destructive_actions_need_a_stopped_space() {
        for action in [Action::ResetSystem, Action::ResetAll, Action::Destroy] {
            assert!(action_enabled(action, Some("stopped")), "{action:?}");
            assert!(!action_enabled(action, Some("running")), "{action:?}");
            assert!(!action_enabled(action, Some("unresponsive")), "{action:?}");
        }
    }

    // подменять backing живого QEMU — порча данных; тот же гвард, что у reset-операций
    #[test]
    fn update_image_needs_a_stopped_space() {
        assert!(action_enabled(Action::UpdateImage, Some("stopped")));
        assert!(!action_enabled(Action::UpdateImage, Some("running")));
        assert!(!action_enabled(Action::UpdateImage, Some("unresponsive")));
    }

    // ради этого размыкали круг: без единого спейса образ всё равно надо чем-то собрать
    #[test]
    fn build_is_offered_without_a_selected_space() {
        assert!(action_enabled(Action::Build, None));
    }

    // состояние, которого демон не присылал, не повод обещать операцию
    #[test]
    fn unknown_state_offers_nothing_but_build() {
        for action in [
            Action::Start,
            Action::Stop,
            Action::OpenWindow,
            Action::ResetSystem,
            Action::ResetAll,
            Action::UpdateImage,
            Action::Destroy,
        ] {
            assert!(!action_enabled(action, None), "{action:?}");
            assert!(!action_enabled(action, Some("бог знает что")), "{action:?}");
        }
    }

    #[test]
    fn slug_error_empty_is_rejected() {
        assert!(slug_error("").is_some());
    }

    #[test]
    fn slug_error_uppercase_is_rejected() {
        assert!(slug_error("Kali").is_some());
    }

    #[test]
    fn slug_error_dot_is_rejected() {
        assert!(slug_error("ka.li").is_some());
    }

    #[test]
    fn slug_error_too_long_is_rejected() {
        assert!(slug_error(&"a".repeat(33)).is_some());
    }

    #[test]
    fn slug_error_valid_slug_is_accepted() {
        assert_eq!(slug_error("kali-2"), None);
    }

    #[test]
    fn id_error_rejects_already_existing() {
        let existing = vec!["kali".to_string(), "telegram".to_string()];
        assert!(id_error("kali", &existing).is_some());
    }

    #[test]
    fn id_error_accepts_unique_valid_id() {
        let existing = vec!["kali".to_string()];
        assert_eq!(id_error("telegram", &existing), None);
    }

    #[test]
    fn id_error_reports_format_before_uniqueness() {
        // с точкой и уже занято — но сообщение всё равно про формат, не про занятость
        let existing = vec!["ka.li".to_string()];
        let err = id_error("ka.li", &existing).unwrap();
        assert!(!err.contains("существует"), "{err}");
    }

    #[test]
    fn is_rrggbb_accepts_valid_hex() {
        assert!(is_rrggbb("#2f9e44"));
    }

    #[test]
    fn is_rrggbb_rejects_missing_hash() {
        assert!(!is_rrggbb("2f9e44"));
    }

    #[test]
    fn is_rrggbb_rejects_wrong_length() {
        assert!(!is_rrggbb("#1234567"));
    }

    #[test]
    fn is_rrggbb_rejects_non_hex() {
        assert!(!is_rrggbb("#zzzzzz"));
    }

    #[test]
    fn color_error_none_for_valid() {
        assert_eq!(color_error("#2f9e44"), None);
    }

    #[test]
    fn color_error_some_for_invalid() {
        assert!(color_error("red").is_some());
    }

    #[test]
    fn preset_labels_are_all_valid_slugs() {
        for label in PRESET_LABELS {
            assert_eq!(slug_error(label), None, "{label}");
        }
    }

    #[test]
    fn preset_colors_are_all_valid_and_distinct() {
        for color in PRESET_COLORS {
            assert!(is_rrggbb(color), "{color}");
        }
        let mut sorted = PRESET_COLORS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            PRESET_COLORS.len(),
            "цвета не должны повторяться"
        );
    }

    fn ready_profile_row() -> view::ProfileRow {
        view::ProfileRow {
            profile: "spike".to_string(),
            template: true,
            manifest_ok: true,
            isolation_level: Some("standard".to_string()),
            manifest_error: None,
        }
    }

    #[test]
    fn profile_eligibility_ready_when_template_and_manifest_ok() {
        assert!(matches!(
            profile_eligibility(&ready_profile_row()),
            ProfileEligibility::Ready
        ));
    }

    #[test]
    fn profile_eligibility_no_template_when_image_missing() {
        let mut row = ready_profile_row();
        row.template = false;
        assert!(matches!(
            profile_eligibility(&row),
            ProfileEligibility::NoTemplate
        ));
    }

    #[test]
    fn profile_eligibility_manifest_broken_carries_error_verbatim() {
        let mut row = ready_profile_row();
        row.manifest_ok = false;
        row.template = false;
        row.isolation_level = None;
        row.manifest_error = Some("некорректный TOML манифеста: ...".to_string());
        match profile_eligibility(&row) {
            ProfileEligibility::ManifestBroken(err) => {
                assert_eq!(err, "некорректный TOML манифеста: ...")
            }
            _ => panic!("ожидался ManifestBroken"),
        }
    }

    #[test]
    fn profile_eligibility_prefers_manifest_broken_over_no_template() {
        // сломанный манифест важнее отсутствия образа — про это и есть тест
        let mut row = ready_profile_row();
        row.manifest_ok = false;
        row.template = true;
        assert!(matches!(
            profile_eligibility(&row),
            ProfileEligibility::ManifestBroken(_)
        ));
    }

    #[test]
    fn profile_option_text_ready_shows_name_and_level() {
        let text = profile_option_text(&ready_profile_row());
        assert!(text.contains("spike"));
        assert!(text.contains("standard"));
        assert!(!text.contains("не собран"));
    }

    #[test]
    fn profile_option_text_no_template_marks_it() {
        let mut row = ready_profile_row();
        row.template = false;
        let text = profile_option_text(&row);
        assert!(text.contains("образ не собран"));
    }

    #[test]
    fn profile_option_text_broken_manifest_marks_it_and_has_no_stale_level() {
        let mut row = ready_profile_row();
        row.manifest_ok = false;
        row.template = false;
        row.isolation_level = None;
        let text = profile_option_text(&row);
        assert!(text.contains("манифест не читается"));
        assert!(!text.contains("standard"));
    }

    #[test]
    fn confirm_message_contains_space_id() {
        for kind in [
            ConfirmKind::Destroy,
            ConfirmKind::ResetAll,
            ConfirmKind::ResetSystem,
            ConfirmKind::UpdateImage,
        ] {
            let text = confirm_message(&kind, "kali");
            assert!(text.contains("kali"), "{text}");
        }
    }

    #[test]
    fn confirm_message_marks_destroy_and_reset_all_irreversible() {
        assert!(confirm_message(&ConfirmKind::Destroy, "x").contains("необратимо"));
        assert!(confirm_message(&ConfirmKind::ResetAll, "x").contains("необратимо"));
    }

    #[test]
    fn confirm_message_reset_system_does_not_claim_irreversibility() {
        assert!(!confirm_message(&ConfirmKind::ResetSystem, "x").contains("необратимо"));
    }

    // предупреждение, не тревога (как у reset-system) — данные не пропадают, но нужно назвать цену словами
    #[test]
    fn confirm_message_update_image_names_the_loss_and_spares_data() {
        let text = confirm_message(&ConfirmKind::UpdateImage, "x");
        assert!(!text.contains("необратимо"), "{text}");
        assert!(text.contains("/data"), "{text}");
        assert!(text.contains("вручную"), "{text}");
    }

    fn sample_spaces() -> Vec<view::SpaceRow> {
        vec![view::SpaceRow {
            id: "kali".to_string(),
            label: "Kali".to_string(),
            color: "#557c94".to_string(),
            profile: "kali-profile".to_string(),
            state: "stopped".to_string(),
            level: "standard".to_string(),
            level_reason: String::new(),
            encrypted: false,
        }]
    }

    #[test]
    fn profile_for_build_finds_profile_of_selected_space() {
        let spaces = sample_spaces();
        assert_eq!(
            profile_for_build(&spaces, "kali"),
            Some("kali-profile".to_string())
        );
    }

    #[test]
    fn profile_for_build_none_when_id_not_present() {
        let spaces = sample_spaces();
        assert_eq!(profile_for_build(&spaces, "missing"), None);
    }

    #[test]
    fn normalize_profile_input_trims_surrounding_whitespace() {
        assert_eq!(
            normalize_profile_input("  kali  "),
            Some("kali".to_string())
        );
    }

    #[test]
    fn normalize_profile_input_rejects_empty_string() {
        assert_eq!(normalize_profile_input(""), None);
    }

    #[test]
    fn normalize_profile_input_rejects_whitespace_only() {
        assert_eq!(normalize_profile_input("   "), None);
    }

    #[test]
    fn passphrase_error_none_when_both_fields_empty() {
        assert_eq!(passphrase_error("", ""), None);
    }

    #[test]
    fn passphrase_error_none_when_passphrase_and_confirm_match() {
        assert_eq!(passphrase_error("secret", "secret"), None);
    }

    #[test]
    fn passphrase_error_some_when_mismatched() {
        assert!(passphrase_error("secret", "other").is_some());
    }

    // забыть подтверждение — тоже расхождение, а не «пароль без подтверждения» по умолчанию
    #[test]
    fn passphrase_error_some_when_only_one_field_filled() {
        assert!(passphrase_error("secret", "").is_some());
        assert!(passphrase_error("", "secret").is_some());
    }

    fn encrypted_space(id: &str) -> view::SpaceRow {
        view::SpaceRow {
            id: id.to_string(),
            label: id.to_string(),
            color: "#557c94".to_string(),
            profile: "kali-profile".to_string(),
            state: "stopped".to_string(),
            level: "standard".to_string(),
            level_reason: String::new(),
            encrypted: true,
        }
    }

    #[test]
    fn needs_passphrase_true_for_start_on_encrypted_space() {
        let spaces = vec![encrypted_space("kali")];
        assert!(needs_passphrase("start", &spaces, "kali"));
    }

    #[test]
    fn needs_passphrase_false_for_start_on_plain_space() {
        let spaces = sample_spaces(); // не зашифрован
        assert!(!needs_passphrase("start", &spaces, "kali"));
    }

    // destroy пароля не получает и не требует ни при каком шифровании — тот же контракт, что у демона
    #[test]
    fn needs_passphrase_false_for_destroy_even_on_encrypted_space() {
        let spaces = vec![encrypted_space("kali")];
        assert!(!needs_passphrase("destroy", &spaces, "kali"));
    }

    #[test]
    fn needs_passphrase_true_for_reset_and_update_ops_on_encrypted_space() {
        let spaces = vec![encrypted_space("kali")];
        for op in ["reset-system", "reset-all", "update-image"] {
            assert!(needs_passphrase(op, &spaces, "kali"), "{op}");
        }
    }

    #[test]
    fn needs_passphrase_false_when_space_id_not_found() {
        let spaces = vec![encrypted_space("kali")];
        assert!(!needs_passphrase("start", &spaces, "missing"));
    }
}
