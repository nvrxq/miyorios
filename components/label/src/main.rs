#![forbid(unsafe_code)]

mod bar;
mod font;
mod font_data;
mod niri;
mod outputs;
mod space;
mod strip;

use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel,
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_output, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/run/miyorios/control.sock";
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const BAR_HEIGHT: u32 = 24;
// только для первого выделения shm-пула — при неверной догадке SlotPool сам подрастёт
const INITIAL_WIDTH_GUESS: u32 = 1920;
// тёмно-красный: демон не ответил — это не обычное состояние полосы, и путать его со спейсом нельзя
const DAEMON_ERROR_BACKGROUND: u32 = 0xffb0_2020;

fn main() -> Result<()> {
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut proc_dir = PathBuf::from("/proc");
    let mut dry_run = false;
    let mut seconds: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--socket" => socket = args.next().context("--socket без значения")?.into(),
            "--proc-dir" => proc_dir = args.next().context("--proc-dir без значения")?.into(),
            "--seconds" => {
                let value = args.next().context("--seconds без значения")?;
                seconds = Some(value.parse().context("--seconds не число")?);
            }
            other => anyhow::bail!("неизвестный аргумент {other}"),
        }
    }

    if dry_run {
        return run_dry_run(&socket, &proc_dir);
    }

    run_bar(socket, proc_dir, seconds)
}

fn run_dry_run(socket: &Path, proc_dir: &Path) -> Result<()> {
    let mut map = niri::WindowMap::default();
    let mut last = String::new();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("не читается поток событий")?;
        if line.trim().is_empty() {
            continue;
        }
        // поток событий — от niri, но повод остановить полосу это слабый: она молчит лишь о том, чего не поняла
        let event = match niri::parse_event(&line) {
            Ok(event) => event,
            Err(err) => {
                eprintln!("miyori-label: {err:#}");
                continue;
            }
        };
        map.apply(event);

        let current = render(&map, socket, proc_dir);
        if current != last {
            println!("{current}");
            last = current;
        }
    }
    Ok(())
}

fn render(map: &niri::WindowMap, socket: &Path, proc_dir: &Path) -> String {
    let spaces = match spaces_from_daemon(socket) {
        Ok(spaces) => spaces,
        // пустой список превратил бы каждое окно спейса в "unknown" — это другая ложь, а не отсутствие ответа
        Err(err) => return format!("STRIP daemon-error {err:#}"),
    };
    match strip::strip_for(map.focus(), proc_dir, &spaces) {
        strip::Strip::Host => "STRIP host".to_string(),
        strip::Strip::Space { id, color, level } => format!("STRIP space {id} {color} {level}"),
        strip::Strip::Orphan { id } => format!("STRIP orphan {id}"),
        strip::Strip::Unknown { id } => format!("STRIP unknown {id}"),
        strip::Strip::Unverified { claimed } => format!("STRIP unverified {claimed}"),
        strip::Strip::FocusUnknown => "STRIP focus-unknown".to_string(),
    }
}

fn spaces_from_daemon(socket: &Path) -> Result<Vec<strip::SpaceInfo>> {
    let reply = miyori_proto::client::call(
        socket,
        &serde_json::json!({"op": "list"}),
        LIST_TIMEOUT,
        &mut |_| {},
    )?;
    let data = match reply {
        miyori_proto::client::Reply::Ok(data) => data,
        miyori_proto::client::Reply::Err { code, message } => {
            anyhow::bail!("{code}: {message}")
        }
    };
    let items = data.as_array().context("list вернул не массив")?;
    items
        .iter()
        .map(|item| {
            let field = |name: &str| -> Result<String> {
                Ok(item
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .with_context(|| format!("спейс без поля {name}"))?
                    .to_string())
            };
            Ok(strip::SpaceInfo {
                id: field("id")?,
                color: field("color")?,
                level: field("isolation-level")?,
                state: field("state")?,
            })
        })
        .collect()
}

// поток niri msg кончился ровно тогда, когда падает Sender — второго сигнала для этого не нужно
fn spawn_niri_reader(sender: channel::Sender<String>) -> Result<Child> {
    let mut child = Command::new("niri")
        .args(["msg", "--json", "event-stream"])
        .stdout(Stdio::piped())
        .spawn()
        .context("не удалось запустить niri msg --json event-stream")?;
    let stdout = child.stdout.take().context("niri msg запущен без stdout")?;
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    Ok(child)
}

// одна полоса, привязанная к своему выходу: у каждого выхода — свои размеры и своя фаза первого configure
struct Bar {
    layer: LayerSurface,
    width: u32,
    height: u32,
    first_configure: bool,
}

impl Bar {
    fn draw(&mut self, pool: &mut SlotPool, content: &bar::BarContent) {
        let width = self.width as i32;
        let height = self.height as i32;
        let stride = width * 4;
        let (buffer, canvas) =
            match pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888) {
                Ok(pair) => pair,
                Err(err) => {
                    eprintln!("miyori-label: не удалось создать буфер отрисовки: {err}");
                    return;
                }
            };

        let mut pixels = vec![content.background; (self.width * self.height) as usize];
        // отступ слева тот же 8, что и справа: обрезка считается по тому, что реально влезает
        let text = font::fit_text(&content.text, (self.width as usize).saturating_sub(16));
        font::draw_text(
            &mut pixels,
            self.width as usize,
            self.height as usize,
            8,
            2,
            &text,
            content.foreground,
        );
        for (chunk, pixel) in canvas.chunks_exact_mut(4).zip(pixels.iter()) {
            chunk.copy_from_slice(&pixel.to_ne_bytes());
        }

        self.layer.wl_surface().damage_buffer(0, 0, width, height);
        if let Err(err) = buffer.attach_to(self.layer.wl_surface()) {
            eprintln!("miyori-label: не удалось прикрепить буфер к поверхности: {err}");
            return;
        }
        self.layer.commit();
    }
}

struct AppData {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    bars: outputs::OutputBars<wl_output::WlOutput, Bar>,
    niri_map: niri::WindowMap,
    last_content: Option<bar::BarContent>,
    socket: PathBuf,
    proc_dir: PathBuf,
    fatal: Option<String>,
    done: bool,
}

impl AppData {
    fn compute_content(&self) -> bar::BarContent {
        match spaces_from_daemon(&self.socket) {
            Ok(spaces) => {
                let strip = strip::strip_for(self.niri_map.focus(), &self.proc_dir, &spaces);
                bar::bar_content(&strip)
            }
            // демон не ответил -> полоса обязана написать это дословно, а не промолчать пустым списком;
            // само сообщение об ошибке уже говорит "демон не отвечает на <путь>" (miyori-proto/src/client.rs)
            Err(err) => bar::BarContent {
                text: format!("{err:#}"),
                background: DAEMON_ERROR_BACKGROUND,
                foreground: 0xffff_ffff,
            },
        }
    }

    fn handle_niri_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match niri::parse_event(line) {
            Ok(event) => self.niri_map.apply(event),
            Err(err) => {
                eprintln!("miyori-label: {err:#}");
                return;
            }
        }
        self.maybe_redraw();
    }

    fn maybe_redraw(&mut self) {
        let content = self.compute_content();
        if self.last_content.as_ref() == Some(&content) {
            return;
        }
        self.last_content = Some(content);
        self.redraw_all();
    }

    // содержимое одно на сессию (состояние фокуса общее) — при смене перерисовываются все полосы разом
    fn redraw_all(&mut self) {
        let Some(content) = self.last_content.clone() else {
            return;
        };
        for bar in self.bars.iter_mut() {
            if bar.first_configure {
                continue; // без согласованных размеров рисовать нечем — своё получит на первом configure
            }
            bar.draw(&mut self.pool, &content);
        }
    }
}

impl CompositorHandler for AppData {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // полоса не анимируется и колбэк кадра не запрашивает — сюда компоситор не зайдёт
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.bars.contains(&output) {
            return; // библиотека гарантирует new_output раз на выход, но вторую полосу заводить всё равно нельзя
        }

        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("miyori-label"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, BAR_HEIGHT);
        layer.set_exclusive_zone(BAR_HEIGHT as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();

        self.bars.insert(
            output,
            Bar {
                layer,
                width: INITIAL_WIDTH_GUESS,
                height: BAR_HEIGHT,
                first_configure: true,
            },
        );
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.bars.remove(&output); // Drop у LayerSurface сам закрывает поверхность и отпускает буфер
    }
}

impl LayerShellHandler for AppData {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // закрытая поверхность с экрана исчезла, а не застыла на старом состоянии: врать нечем,
        // и ронять из-за одного монитора полосы на остальных — хуже, чем потерять одну
        self.bars.remove_matching(|bar| &bar.layer == layer);
        if self.bars.is_empty() {
            self.fatal
                .get_or_insert_with(|| "все поверхности полосы закрыты композитором".to_string());
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // считаем не больше раза за сессию: у новых полос при их первом configure — то же содержимое, что у остальных
        if self.last_content.is_none() {
            self.last_content = Some(self.compute_content());
        }
        let content = self.last_content.clone();

        let Some(bar) = self.bars.find_mut(|bar| &bar.layer == layer) else {
            return; // configure для уже удалённого выхода — устаревшее событие
        };

        let (w, h) = configure.new_size;
        if w != 0 {
            bar.width = w;
        }
        bar.height = if h != 0 { h } else { BAR_HEIGHT };

        if !bar.first_configure {
            return;
        }
        bar.first_configure = false;
        if let Some(content) = content {
            bar.draw(&mut self.pool, &content);
        }
    }
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

smithay_client_toolkit::delegate_registry!(AppData);

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(AppData);

fn run_bar(socket: PathBuf, proc_dir: PathBuf, seconds: Option<u64>) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("не удалось подключиться к Wayland (проверьте WAYLAND_DISPLAY)")?;
    let (globals, mut event_queue) =
        registry_queue_init::<AppData>(&conn).context("не удалось перечислить глобалы Wayland")?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<AppData> =
        EventLoop::try_new().context("не удалось создать цикл событий calloop")?;

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor недоступен")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1 недоступен")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm недоступен")?;

    let pool = SlotPool::new((INITIAL_WIDTH_GUESS * BAR_HEIGHT * 4) as usize, &shm)
        .context("не удалось создать пул shm")?;

    let mut app_data = AppData {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        layer_shell,
        shm,
        pool,
        bars: outputs::OutputBars::new(),
        niri_map: niri::WindowMap::default(),
        last_content: None,
        socket,
        proc_dir,
        fatal: None,
        done: false,
    };

    // выходы, уже существовавшие на момент запуска, OutputState отдаёт только после round-trip;
    // приходят они тем же new_output, что и появившиеся позже, — полоса заводится одним и тем же путём
    event_queue
        .roundtrip(&mut app_data)
        .map_err(|err| anyhow::anyhow!("не удалось получить список выходов Wayland: {err}"))?;

    let (sender, receiver) = channel::channel::<String>();
    let mut niri_child = spawn_niri_reader(sender)?;

    event_loop
        .handle()
        .insert_source(receiver, |event, _, app_data: &mut AppData| match event {
            channel::Event::Msg(line) => app_data.handle_niri_line(&line),
            // niri msg умер или закрыл поток — застывшая полоса врала бы, поэтому дальше только выход
            channel::Event::Closed => {
                app_data.fatal.get_or_insert_with(|| {
                    "niri msg event-stream: поток событий закончился".to_string()
                });
            }
        })
        .map_err(|err| anyhow::anyhow!("не удалось подключить канал niri: {err}"))?;

    if let Some(secs) = seconds {
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(Duration::from_secs(secs)),
                |_, _, app_data: &mut AppData| {
                    app_data.done = true;
                    TimeoutAction::Drop
                },
            )
            .map_err(|err| anyhow::anyhow!("не удалось поставить таймер --seconds: {err}"))?;
    }

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|err| anyhow::anyhow!("не удалось подключить соединение Wayland: {err}"))?;

    let outcome = loop {
        if let Err(err) = event_loop.dispatch(Duration::from_millis(50), &mut app_data) {
            break Err(anyhow::anyhow!("сбой цикла событий Wayland: {err}"));
        }
        if let Some(reason) = app_data.fatal.take() {
            break Err(anyhow::anyhow!(reason));
        }
        if app_data.done {
            break Ok(());
        }
    };

    let _ = niri_child.kill();
    let _ = niri_child.wait();
    outcome
}
