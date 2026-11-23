// IP Cam Webcam - native libadwaita app.
// Controls DigitallyRefined/android-ip-camera and feeds its stream to a
// v4l2loopback device through ffmpeg.

use adw::prelude::*;
use base64::Engine;
use gtk::{gio, glib};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

// ---------------------------------------------------------------- options ---
const CODECS: [&str; 2] = ["h264", "mjpeg"];
const DECODERS: [&str; 7] = [
    "auto (software)",
    "vaapi (Intel/AMD)",
    "cuda/nvdec (NVIDIA)",
    "h264_cuvid (NVIDIA)",
    "h264_qsv (Intel)",
    "mjpeg_cuvid (NVIDIA)",
    "mjpeg_qsv (Intel)",
];
const PIX_FMTS: [&str; 6] = ["yuyv422", "yuv420p", "nv12", "rgb24", "bgr24", "gray"];
const CAMERAS: [&str; 3] = ["back", "front", "toggle"];
const ROTATIONS: [&str; 4] = ["0", "90", "180", "270"];
const PRESET_RES: [&str; 5] = ["low", "medium", "high", "auto", "max"];

fn decoder_args(name: &str) -> Vec<&'static str> {
    match name {
        "vaapi (Intel/AMD)" => vec!["-hwaccel", "vaapi", "-hwaccel_device", "/dev/dri/renderD128"],
        "cuda/nvdec (NVIDIA)" => vec!["-hwaccel", "cuda"],
        "h264_cuvid (NVIDIA)" => vec!["-c:v", "h264_cuvid"],
        "h264_qsv (Intel)" => vec!["-c:v", "h264_qsv"],
        "mjpeg_cuvid (NVIDIA)" => vec!["-c:v", "mjpeg_cuvid"],
        "mjpeg_qsv (Intel)" => vec!["-c:v", "mjpeg_qsv"],
        _ => vec![],
    }
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Hide the password in a URL before showing it in the log.
fn mask(arg: &str) -> String {
    if let (Some(a), Some(at)) = (arg.find("://"), arg.rfind('@')) {
        if let Some(colon) = arg[a + 3..at].find(':') {
            return format!("{}:****{}", &arg[..a + 3 + colon], &arg[at..]);
        }
    }
    arg.to_string()
}

fn conf_path() -> PathBuf {
    glib::home_dir().join(".config/ipcam-webcam.json")
}

// ------------------------------------------------------------- phone API ----
#[derive(Clone)]
struct Phone {
    host: String,
    port: u32,
    user: String,
    pass: String,
    tls: bool,
}

impl Phone {
    fn base(&self) -> String {
        format!("{}://{}:{}", if self.tls { "https" } else { "http" }, self.host, self.port)
    }

    fn token(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.user, self.pass))
    }

    /// Stream URL with percent-encoded credentials in the userinfo part, exactly like the
    /// app's README does for ffmpeg/go2rtc. ffmpeg then answers whatever auth scheme
    /// (Basic or Digest) the server challenges with.
    fn stream_url(&self, codec: &str) -> String {
        let path = if codec == "mjpeg" { "/video/mjpeg" } else { "/video/h264" };
        let auth = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}:{}@", pct(&self.user), pct(&self.pass))
        };
        format!("{}://{}{}:{}{}", if self.tls { "https" } else { "http" }, auth, self.host, self.port, path)
    }

    fn call(&self, method: &str, path: &str, q: &[(&str, String)], timeout: u64) -> Result<Vec<u8>, String> {
        // The app generates a self-signed certificate by default, so don't verify it.
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| e.to_string())?;
        let agent = ureq::AgentBuilder::new()
            .tls_connector(Arc::new(tls))
            .timeout(Duration::from_secs(timeout))
            .build();
        let mut req = agent.request(method, &format!("{}{}", self.base(), path));
        for (k, v) in q {
            req = req.query(k, v);
        }
        if !self.user.is_empty() {
            req = req.set("Authorization", &format!("Basic {}", self.token()));
        }
        let resp = if method == "POST" { req.send_bytes(&[]) } else { req.call() }.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }

    /// /?torch=on&zoom=2.0 ...
    fn set(&self, key: &str, value: String) -> Result<(), String> {
        self.call("GET", "/", &[(key, value)], 6).map(|_| ())
    }

    fn info(&self) -> Result<serde_json::Value, String> {
        let body = self.call("GET", "/info.json", &[], 6)?;
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }

    fn snapshot(&self, path: &PathBuf) -> Result<(), String> {
        let data = self.call("GET", "/video/snapshot", &[], 15)?;
        std::fs::write(path, data).map_err(|e| e.to_string())
    }

    fn rec(&self, action: &str) -> Result<String, String> {
        let body = self.call("POST", &format!("/record/{action}"), &[], 8)?;
        Ok(String::from_utf8_lossy(&body).to_string())
    }
}

// ---------------------------------------------------------------- ffmpeg ----
fn build_cmd(p: &Phone, codec: &str, decoder: &str, device: &str, pix: &str, size: &str, fps: &str) -> Vec<String> {
    let mut c: Vec<String> = [
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-nostdin", "-fflags", "nobuffer", "-flags",
        "low_delay", "-probesize", "65536", "-analyzeduration", "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if p.tls {
        c.extend(["-tls_verify", "0"].map(String::from));
    }
    if codec == "h264" {
        c.extend(["-f", "h264"].map(String::from)); // raw Annex-B: nothing to probe
    }
    c.extend(decoder_args(decoder).into_iter().map(String::from));
    c.push("-i".into());
    c.push(p.stream_url(codec));

    let mut vf: Vec<String> = vec![];
    if !size.is_empty() {
        vf.push(format!("scale={}", size.replace('x', ":")));
    }
    if !fps.is_empty() {
        vf.push(format!("fps={fps}"));
    }
    vf.push(format!("format={pix}")); // also downloads hw frames to system memory
    c.extend(["-vf".to_string(), vf.join(","), "-an".into(), "-c:v".into(), "rawvideo".into()]);
    c.extend(["-pix_fmt".to_string(), pix.to_string(), "-f".into(), "v4l2".into(), device.to_string()]);
    c
}

struct Streamer {
    child: Arc<Mutex<Option<Child>>>,
    want: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
}

impl Streamer {
    fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            want: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn running(&self) -> bool {
        self.want.load(Ordering::SeqCst)
    }

    fn start(&self, cmd: Vec<String>, auto_restart: bool, tx: async_channel::Sender<String>) {
        self.stop();
        self.want.store(true, Ordering::SeqCst);
        let my = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (child, want, generation) = (self.child.clone(), self.want.clone(), self.generation.clone());
        std::thread::spawn(move || {
            let current = || want.load(Ordering::SeqCst) && generation.load(Ordering::SeqCst) == my;
            let mut fails: u32 = 0;
            while current() {
                let started = std::time::Instant::now();
                let spawned = Command::new(&cmd[0])
                    .args(&cmd[1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn();
                let mut c = match spawned {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send_blocking(format!("could not start ffmpeg: {e}"));
                        break;
                    }
                };
                let stderr = c.stderr.take();
                *child.lock().unwrap() = Some(c);
                if !current() {
                    if let Some(c) = child.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                }
                let mut fatal: Option<&str> = None;
                if let Some(s) = stderr {
                    for line in BufReader::new(s).lines().map_while(Result::ok) {
                        if line.contains("401 Unauthorized") || line.contains("403 Forbidden") {
                            fatal = Some("Authentication failed (HTTP 401/403): check username and password. Not retrying, to avoid being locked out.");
                        } else if line.contains("429 Too Many") {
                            fatal = Some("The phone is rate-limiting this client (HTTP 429), probably after failed logins. Wait a minute, then start again.");
                        }
                        let _ = tx.send_blocking(mask(&line));
                    }
                }
                let code = child.lock().unwrap().take().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
                let _ = tx.send_blocking(format!("ffmpeg exited ({code:?})"));
                if let Some(msg) = fatal {
                    let _ = tx.send_blocking(msg.to_string());
                    break;
                }
                if !(auto_restart && current()) {
                    break;
                }
                // back off: 2s, 4s, 8s ... 30s; a run that lasted a while resets it
                fails = if started.elapsed() > Duration::from_secs(15) { 0 } else { fails + 1 };
                let wait = (2u64 << fails.min(4)).min(30);
                let _ = tx.send_blocking(format!("restarting in {wait}s..."));
                std::thread::sleep(Duration::from_secs(wait));
            }
            if generation.load(Ordering::SeqCst) == my {
                want.store(false, Ordering::SeqCst);
            }
        });
    }

    fn stop(&self) {
        self.want.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

// ------------------------------------------------------------ widget utils --
fn entry(title: &str, text: &str) -> adw::EntryRow {
    let r = adw::EntryRow::builder().title(title).build();
    r.set_text(text);
    r
}

fn switch(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    adw::SwitchRow::builder().title(title).subtitle(subtitle).active(active).build()
}

fn combo(title: &str, subtitle: &str, items: &[&str], sel: &str) -> adw::ComboRow {
    let r = adw::ComboRow::builder().title(title).subtitle(subtitle).model(&gtk::StringList::new(items)).build();
    if let Some(i) = items.iter().position(|x| *x == sel) {
        r.set_selected(i as u32);
    }
    r
}

fn selected(r: &adw::ComboRow) -> String {
    r.selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|s| s.string().to_string())
        .unwrap_or_default()
}

fn spin(title: &str, subtitle: &str, lo: f64, hi: f64, step: f64, val: f64, digits: u32) -> adw::SpinRow {
    let r = adw::SpinRow::with_range(lo, hi, step);
    r.set_title(title);
    r.set_subtitle(subtitle);
    r.set_digits(digits);
    r.set_value(val);
    r
}

fn group(title: &str, desc: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).description(desc).build()
}

fn action_row(title: &str, subtitle: &str, buttons: &[&gtk::Button]) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    for b in buttons {
        r.add_suffix(*b);
    }
    r
}

fn button(label: &str, css: Option<&str>) -> gtk::Button {
    let b = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
    if let Some(c) = css {
        b.add_css_class(c);
    }
    b
}

// ------------------------------------------------------------------- UI -----
struct Ui {
    win: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: adw::ViewStack,
    start_btn: gtk::Button,
    // phone
    host: adw::EntryRow,
    port: adw::SpinRow,
    user: adw::EntryRow,
    password: adw::PasswordEntryRow,
    tls: adw::SwitchRow,
    connect_btn: gtk::Button,
    panel_btn: gtk::Button,
    // controls
    camera: adw::ComboRow,
    res: adw::ComboRow,
    rotate: adw::ComboRow,
    mirror: adw::SwitchRow,
    zoom: adw::SpinRow,
    exposure: adw::SpinRow,
    contrast: adw::SpinRow,
    focus: adw::SpinRow,
    autofocus: adw::SwitchRow,
    torch: adw::SwitchRow,
    rec_btn: gtk::Button,
    recstop_btn: gtk::Button,
    snap_btn: gtk::Button,
    // webcam
    codec: adw::ComboRow,
    decoder: adw::ComboRow,
    pix: adw::ComboRow,
    size: adw::EntryRow,
    fps: adw::EntryRow,
    autorestart: adw::SwitchRow,
    refresh_pw: adw::SwitchRow,
    refresh_btn: gtk::Button,
    device: adw::EntryRow,
    module_btn: gtk::Button,
    // log
    buf: gtk::TextBuffer,
    log_view: gtk::TextView,
    log_tx: async_channel::Sender<String>,
    // state
    streamer: Streamer,
    loading: Cell<bool>,
    cam_ids: RefCell<Vec<String>>,
    timers: RefCell<HashMap<String, glib::SourceId>>,
}

impl Ui {
    fn new(app: &adw::Application) -> Rc<Ui> {
        let conf: serde_json::Value = std::fs::read_to_string(conf_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let cs = |k: &str, d: &str| conf.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
        let cb = |k: &str, d: bool| conf.get(k).and_then(|v| v.as_bool()).unwrap_or(d);
        let cn = |k: &str, d: f64| conf.get(k).and_then(|v| v.as_f64()).unwrap_or(d);

        // --- camera page
        let host = entry("Host / IP address", &cs("host", ""));
        let port = spin("Port", "", 1.0, 65535.0, 1.0, cn("port", 4444.0), 0);
        let user = entry("Username", &cs("user", ""));
        let password = adw::PasswordEntryRow::builder().title("Password").build();
        password.set_text(&cs("pass", ""));
        let tls = switch("HTTPS", "Self-signed certificates are accepted", cb("tls", true));
        let connect_btn = button("Connect", Some("suggested-action"));
        let panel_btn = button("Web panel", None);

        let g_phone = group("Phone", "Address of the Android IP Camera server");
        g_phone.add(&host);
        g_phone.add(&port);
        g_phone.add(&user);
        g_phone.add(&password);
        g_phone.add(&tls);
        g_phone.add(&action_row("Connection", "Fetch cameras and resolutions", &[&connect_btn, &panel_btn]));

        let camera = combo("Camera", "", &CAMERAS, "back");
        let res = combo("Resolution", "", &PRESET_RES, "auto");
        let rotate = combo("Rotate", "", &ROTATIONS, "0");
        let mirror = switch("Mirror", "", false);
        let zoom = spin("Zoom", "", 1.0, 10.0, 0.1, 1.0, 1);
        let exposure = spin("Exposure", "", -12.0, 12.0, 1.0, 0.0, 0);
        let contrast = spin("Contrast", "", -100.0, 100.0, 1.0, 0.0, 0);
        let focus = spin("Focus distance", "Used when autofocus is off", 0.0, 1.0, 0.01, 0.5, 2);
        focus.set_sensitive(false);
        let autofocus = switch("Autofocus", "", true);
        let torch = switch("Torch", "", false);

        let g_ctl = group("Camera controls", "Applied on the phone immediately");
        for r in [&camera, &res, &rotate].map(|r| r.upcast_ref::<gtk::Widget>().clone()) {
            g_ctl.add(&r);
        }
        g_ctl.add(&mirror);
        g_ctl.add(&zoom);
        g_ctl.add(&exposure);
        g_ctl.add(&contrast);
        g_ctl.add(&autofocus);
        g_ctl.add(&focus);
        g_ctl.add(&torch);

        let rec_btn = button("Record", None);
        let recstop_btn = button("Stop", None);
        let snap_btn = button("Take", None);
        let g_cap = group("Capture", "");
        g_cap.add(&action_row("Recording", "Record to the phone's storage (MP4)", &[&rec_btn, &recstop_btn]));
        g_cap.add(&action_row("Snapshot", "Save a full-resolution JPEG to Pictures", &[&snap_btn]));

        let page_cam = adw::PreferencesPage::new();
        page_cam.add(&g_phone);
        page_cam.add(&g_ctl);
        page_cam.add(&g_cap);

        // --- webcam page
        let codec = combo("Source stream", "H.264: low bandwidth · MJPEG: low latency, low CPU", &CODECS, &cs("codec", "h264"));
        let decoder = combo("Decoder", "Hardware decoders need matching drivers", &DECODERS, &cs("decoder", DECODERS[0]));
        let pix = combo("Pixel format", "", &PIX_FMTS, &cs("pix", "yuyv422"));
        let size = entry("Scale (WxH, blank = native)", &cs("size", ""));
        let fps = entry("Frame rate (blank = native)", &cs("fps", ""));
        let autorestart = switch("Auto-restart", "Reconnect if the stream drops", cb("autorestart", true));
        let device = entry("Device", &cs("device", "/dev/video10"));
        let module_btn = button("Load module", None);
        let refresh_pw = switch(
            "Refresh PipeWire on start",
            "Restarts WirePlumber so GNOME Camera and other PipeWire apps see the device",
            cb("refresh_pw", true),
        );
        let refresh_btn = button("Restart now", None);

        let g_in = group("Input codec", "How the phone stream is fetched and decoded");
        g_in.add(&codec);
        g_in.add(&decoder);
        let g_out = group("Output", "v4l2 devices accept raw video only, so this sets the pixel format");
        g_out.add(&pix);
        g_out.add(&size);
        g_out.add(&fps);
        g_out.add(&autorestart);
        let g_dev = group("Virtual device", "");
        g_dev.add(&device);
        g_dev.add(&action_row("v4l2loopback", "Create the virtual camera (asks for admin rights)", &[&module_btn]));
        g_dev.add(&refresh_pw);
        g_dev.add(&action_row("WirePlumber", "Re-detect the virtual camera (stream must be running)", &[&refresh_btn]));

        let page_web = adw::PreferencesPage::new();
        page_web.add(&g_in);
        page_web.add(&g_out);
        page_web.add(&g_dev);

        // --- log page
        let log_view = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .build();
        let buf = log_view.buffer();
        let sw = gtk::ScrolledWindow::builder().child(&log_view).vexpand(true).build();
        let clear = gtk::Button::builder()
            .label("Clear")
            .halign(gtk::Align::End)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        {
            let b = buf.clone();
            clear.connect_clicked(move |_| b.set_text(""));
        }
        let page_log = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page_log.append(&sw);
        page_log.append(&clear);

        // --- chrome
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&page_cam, Some("camera"), "Camera", "camera-video-symbolic");
        stack.add_titled_with_icon(&page_web, Some("webcam"), "Webcam", "camera-web-symbolic");
        stack.add_titled_with_icon(&page_log, Some("log"), "Log", "utilities-terminal-symbolic");

        let start_btn = button("Start webcam", Some("suggested-action"));
        let header = adw::HeaderBar::new();
        header.pack_start(&start_btn);
        header.set_title_widget(Some(&adw::WindowTitle::new("IP Cam Webcam", "Android → V4L2")));
        let bar = adw::ViewSwitcherBar::builder().stack(&stack).reveal(true).build();
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.add_bottom_bar(&bar);
        tv.set_content(Some(&stack));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&tv));
        let win = adw::ApplicationWindow::builder()
            .application(app)
            .title("IP Cam Webcam")
            .default_width(560)
            .default_height(800)
            .content(&toasts)
            .build();

        let (log_tx, log_rx) = async_channel::unbounded::<String>();
        let ui = Rc::new(Ui {
            win, toasts, stack, start_btn, host, port, user, password, tls, connect_btn, panel_btn, camera, res,
            rotate, mirror, zoom, exposure, contrast, focus, autofocus, torch, rec_btn, recstop_btn, snap_btn,
            codec, decoder, pix, size, fps, autorestart, refresh_pw, refresh_btn, device, module_btn, buf, log_view, log_tx,
            streamer: Streamer::new(),
            loading: Cell::new(false),
            cam_ids: RefCell::new(Vec::new()),
            timers: RefCell::new(HashMap::new()),
        });
        ui.wire();

        // drain log lines (sent from worker threads) on the UI thread
        let u = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(line) = log_rx.recv().await {
                u.append_log(&line);
            }
        });
        // keep the start/stop button honest when ffmpeg dies on its own
        let u = ui.clone();
        glib::timeout_add_seconds_local(1, move || {
            u.sync();
            glib::ControlFlow::Continue
        });
        ui
    }

    // ---- plumbing
    fn phone(&self) -> Phone {
        Phone {
            host: self.host.text().trim().to_string(),
            port: self.port.value() as u32,
            user: self.user.text().to_string(),
            pass: self.password.text().to_string(),
            tls: self.tls.is_active(),
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn log(&self, msg: &str) {
        let _ = self.log_tx.try_send(msg.to_string());
    }

    fn append_log(&self, line: &str) {
        self.buf.insert(&mut self.buf.end_iter(), &format!("{line}\n"));
        let n = self.buf.line_count();
        if n > 2000 {
            if let Some(mut end) = self.buf.iter_at_line(n - 2000) {
                self.buf.delete(&mut self.buf.start_iter(), &mut end);
            }
        }
        let mark = self.buf.create_mark(None, &self.buf.end_iter(), false);
        self.log_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        self.buf.delete_mark(&mark);
    }

    fn debounce(self: &Rc<Self>, key: &str, ms: u64, f: impl Fn() + 'static) {
        if let Some(id) = self.timers.borrow_mut().remove(key) {
            id.remove();
        }
        let ui = self.clone();
        let k = key.to_string();
        let id = glib::timeout_add_local(Duration::from_millis(ms), move || {
            ui.timers.borrow_mut().remove(&k); // dropping a SourceId doesn't remove the source
            f();
            glib::ControlFlow::Break
        });
        self.timers.borrow_mut().insert(key.to_string(), id);
    }

    fn queue_save(self: &Rc<Self>) {
        let ui = self.clone();
        self.debounce("save", 600, move || ui.save());
    }

    /// Run a phone call off the UI thread, then hand the result back on it.
    fn api<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl FnOnce(Phone) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&Rc<Ui>, T) + 'static,
    ) {
        if self.host.text().trim().is_empty() {
            self.toast("Enter the phone's IP address first");
            return;
        }
        let phone = self.phone();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match gio::spawn_blocking(move || work(phone)).await {
                Ok(Ok(v)) => done(&ui, v),
                Ok(Err(e)) => {
                    ui.log(&format!("error: {e}"));
                    ui.toast(&format!("Request failed: {e}"));
                }
                Err(_) => ui.toast("Background task failed"),
            }
        });
    }

    fn send(self: &Rc<Self>, key: &'static str, value: String) {
        self.api(move |p| p.set(key, value), |_, _| {});
    }

    fn save(&self) {
        let d = serde_json::json!({
            "host": self.host.text().to_string(), "port": self.port.value(), "user": self.user.text().to_string(),
            "pass": self.password.text().to_string(), "tls": self.tls.is_active(),
            "codec": selected(&self.codec), "decoder": selected(&self.decoder), "pix": selected(&self.pix),
            "size": self.size.text().to_string(), "fps": self.fps.text().to_string(),
            "device": self.device.text().to_string(),
            "autorestart": self.autorestart.is_active(),
            "refresh_pw": self.refresh_pw.is_active(),
        });
        let path = conf_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&path, d.to_string()).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }

    fn sync(&self) {
        let on = self.streamer.running();
        self.start_btn.set_label(if on { "Stop webcam" } else { "Start webcam" });
        let (add, remove) = if on { ("destructive-action", "suggested-action") } else { ("suggested-action", "destructive-action") };
        self.start_btn.remove_css_class(remove);
        self.start_btn.add_css_class(add);
    }

    // ---- actions
    fn toggle_stream(self: &Rc<Self>) {
        if self.streamer.running() {
            self.streamer.stop();
            self.sync();
            return;
        }
        if self.host.text().trim().is_empty() {
            self.toast("Enter the phone's IP address first");
            return;
        }
        let dev = self.device.text().trim().to_string();
        if !std::path::Path::new(&dev).exists() {
            self.toast(&format!("{dev} not found – load v4l2loopback first"));
            self.stack.set_visible_child_name("webcam");
            return;
        }
        self.save();
        let cmd = build_cmd(
            &self.phone(), &selected(&self.codec), &selected(&self.decoder), &dev, &selected(&self.pix),
            self.size.text().trim(), self.fps.text().trim(),
        );
        let shown: Vec<String> = cmd.iter().map(|c| mask(c)).collect();
        self.log(&format!("$ {}", shown.join(" ")));
        self.streamer.start(cmd, self.autorestart.is_active(), self.log_tx.clone());
        self.toast(&format!("Streaming to {dev}"));
        if self.refresh_pw.is_active() {
            self.refresh_pipewire(3); // give ffmpeg a moment to write the first frame
        }
        self.sync();
    }

    fn connect_phone(self: &Rc<Self>) {
        self.api(|p| p.info(), |ui, info| ui.apply_info(&info));
    }

    /// Fill the controls from the phone's /info.json (cameras, sizes, current lens settings).
    fn apply_info(self: &Rc<Self>, info: &serde_json::Value) {
        let Some(cams) = info.get("cameras").and_then(|c| c.as_array()).filter(|c| !c.is_empty()) else {
            self.toast("The phone reported no cameras");
            return;
        };
        let text = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        let ids: Vec<String> = cams.iter().map(|c| text(c, "id").unwrap_or_default()).collect();
        let labels: Vec<String> =
            cams.iter().map(|c| text(c, "label").or_else(|| text(c, "id")).unwrap_or_default()).collect();
        let current = info.pointer("/settings/cameraId").and_then(|v| v.as_str()).unwrap_or("");
        let idx = ids.iter().position(|i| i == current).unwrap_or(0);
        let cam = &cams[idx];

        // resolution list = presets + the sizes this camera supports
        let sizes: Vec<String> = cam
            .get("sizes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|z| Some(format!("{}x{}", z.get("w")?.as_u64()?, z.get("h")?.as_u64()?)))
                    .collect()
            })
            .unwrap_or_default();
        let mut res_items: Vec<&str> = PRESET_RES.to_vec();
        res_items.extend(sizes.iter().map(|x| x.as_str()));
        let cur_res = info.pointer("/settings/resolution").and_then(|v| v.as_str()).unwrap_or("auto");

        let lens = cam.get("lensSettings");
        let lens_str = |k: &str| lens.and_then(|l| l.get(k)).and_then(|v| v.as_str());
        let lens_num = |k: &str, d: f64| lens_str(k).and_then(|x| x.parse::<f64>().ok()).unwrap_or(d);
        let zoom_lim = |k: &str, d: f64| {
            cam.pointer(&format!("/zoom/{k}")).and_then(|v| v.as_str()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(d)
        };

        self.loading.set(true);
        *self.cam_ids.borrow_mut() = ids;
        let label_refs: Vec<&str> = labels.iter().map(|x| x.as_str()).collect();
        self.camera.set_model(Some(&gtk::StringList::new(&label_refs)));
        self.camera.set_selected(idx as u32);
        self.res.set_model(Some(&gtk::StringList::new(&res_items)));
        self.res.set_selected(res_items.iter().position(|x| *x == cur_res).unwrap_or(3) as u32);
        self.zoom.set_range(zoom_lim("min", 1.0), zoom_lim("max", 10.0));
        self.zoom.set_value(lens_num("zoom", 1.0));
        self.exposure.set_value(lens_num("exposure", 0.0));
        self.contrast.set_value(lens_num("contrast", 0.0));
        self.mirror.set_active(lens_str("mirror") == Some("true"));
        let rot = lens_str("rotate").unwrap_or("0");
        self.rotate.set_selected(ROTATIONS.iter().position(|r| *r == rot).unwrap_or(0) as u32);
        let fd = lens_num("focusDistance", -1.0);
        self.autofocus.set_active(fd < 0.0);
        self.focus.set_sensitive(fd >= 0.0);
        if fd >= 0.0 {
            self.focus.set_value(fd);
        }
        self.torch.set_sensitive(cam.get("hasFlash").and_then(|v| v.as_bool()).unwrap_or(false));
        self.torch.set_active(info.pointer("/settings/torch").and_then(|v| v.as_str()) == Some("on"));
        self.loading.set(false);

        let battery = info.get("batteryPercent").and_then(|v| v.as_u64()).map(|b| format!(" · battery {b}%")).unwrap_or_default();
        self.toast(&format!("Connected · {}{battery}", labels[idx]));
        self.log(&format!("connected; camera {}, {} resolutions{battery}", labels[idx], sizes.len()));
    }

    fn snapshot(self: &Rc<Self>) {
        let dir = glib::user_special_dir(glib::UserDirectory::Pictures).unwrap_or_else(glib::home_dir);
        let name = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("ipcam-%Y%m%d-%H%M%S.jpg").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "ipcam-snapshot.jpg".into());
        let path = dir.join(&name);
        self.api(move |p| p.snapshot(&path), move |ui, _| ui.toast(&format!("Saved {name}")));
    }

    /// PipeWire only notices a v4l2loopback camera once it has frames, so restart
    /// WirePlumber (the session manager that lists cameras) after the stream is up.
    fn refresh_pipewire(self: &Rc<Self>, delay_secs: u64) {
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if delay_secs > 0 {
                glib::timeout_future(Duration::from_secs(delay_secs)).await;
                if !ui.streamer.running() {
                    return;
                }
            }
            let out = gio::spawn_blocking(|| {
                Command::new("systemctl").args(["--user", "restart", "wireplumber"]).output()
            })
            .await;
            let msg = match out {
                Ok(Ok(o)) if o.status.success() => "WirePlumber restarted – the camera should now appear".to_string(),
                Ok(Ok(o)) => format!("WirePlumber restart failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Ok(Err(e)) => format!("systemctl failed: {e}"),
                Err(_) => "background task failed".to_string(),
            };
            ui.log(&msg);
            ui.toast(&msg);
        });
    }

    fn load_module(self: &Rc<Self>) {
        let nr: String = self.device.text().chars().filter(|c| c.is_ascii_digit()).collect();
        let nr = if nr.is_empty() { "10".to_string() } else { nr };
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let out = gio::spawn_blocking(move || {
                Command::new("pkexec")
                    .args(["modprobe", "v4l2loopback", "devices=1", &format!("video_nr={nr}"),
                           "card_label=Android IP Cam", "exclusive_caps=1"])
                    .output()
            })
            .await;
            let msg = match out {
                Ok(Ok(o)) if o.status.success() => "v4l2loopback loaded".to_string(),
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                Ok(Err(e)) => format!("pkexec failed: {e}"),
                Err(_) => "background task failed".to_string(),
            };
            ui.log(&msg);
            ui.toast(msg.lines().next().unwrap_or("done"));
        });
    }

    // ---- signal wiring
    fn wire_spin(self: &Rc<Self>, row: &adw::SpinRow, key: &'static str, digits: usize) {
        let ui = self.clone();
        row.connect_notify_local(Some("value"), move |r, _| {
            if ui.loading.get() {
                return;
            }
            let s = format!("{:.*}", digits, r.value());
            let u = ui.clone();
            ui.debounce(key, 350, move || u.send(key, s.clone()));
        });
    }

    fn wire_combo(self: &Rc<Self>, row: &adw::ComboRow, key: &'static str) {
        let ui = self.clone();
        row.connect_selected_notify(move |r| {
            if !ui.loading.get() {
                ui.send(key, selected(r));
            }
        });
    }

    fn wire(self: &Rc<Self>) {
        let ui = self.clone();
        self.camera.connect_selected_notify(move |r| {
            if ui.loading.get() {
                return;
            }
            let id = ui.cam_ids.borrow().get(r.selected() as usize).cloned().unwrap_or_else(|| selected(r));
            ui.api(
                move |p| {
                    p.set("camera", id)?;
                    p.info()
                },
                |ui, info| ui.apply_info(&info),
            );
        });
        self.wire_combo(&self.res, "resolution");
        self.wire_combo(&self.rotate, "rotate");
        self.wire_spin(&self.zoom, "zoom", 1);
        self.wire_spin(&self.exposure, "exposure", 0);
        self.wire_spin(&self.contrast, "contrast", 0);
        self.wire_spin(&self.focus, "focus_distance", 2);

        let ui = self.clone();
        self.mirror.connect_active_notify(move |r| {
            if !ui.loading.get() {
                ui.send("mirror", r.is_active().to_string());
            }
        });
        let ui = self.clone();
        self.torch.connect_active_notify(move |r| {
            if !ui.loading.get() {
                ui.send("torch", if r.is_active() { "on" } else { "off" }.into());
            }
        });
        let ui = self.clone();
        self.autofocus.connect_active_notify(move |r| {
            ui.focus.set_sensitive(!r.is_active());
            if ui.loading.get() {
                return;
            }
            let v = if r.is_active() { "-1".to_string() } else { format!("{:.2}", ui.focus.value()) };
            ui.send("focus_distance", v);
        });

        let ui = self.clone();
        self.connect_btn.connect_clicked(move |_| ui.connect_phone());
        let ui = self.clone();
        self.panel_btn.connect_clicked(move |_| {
            let _ = gio::AppInfo::launch_default_for_uri(&format!("{}/", ui.phone().base()), None::<&gio::AppLaunchContext>);
        });
        let ui = self.clone();
        self.rec_btn.connect_clicked(move |_| {
            ui.api(|p| p.rec("start"), |ui, s| {
                ui.toast("Recording started");
                ui.log(&s);
            })
        });
        let ui = self.clone();
        self.recstop_btn.connect_clicked(move |_| {
            ui.api(|p| p.rec("stop"), |ui, s| {
                ui.toast("Recording stopped");
                ui.log(&s);
            })
        });
        let ui = self.clone();
        self.snap_btn.connect_clicked(move |_| ui.snapshot());
        let ui = self.clone();
        self.module_btn.connect_clicked(move |_| ui.load_module());
        let ui = self.clone();
        self.refresh_btn.connect_clicked(move |_| ui.refresh_pipewire(0));
        let ui = self.clone();
        self.start_btn.connect_clicked(move |_| ui.toggle_stream());

        // persist connection / webcam settings
        for e in [&self.host, &self.user, &self.size, &self.fps, &self.device] {
            let ui = self.clone();
            e.connect_changed(move |_| ui.queue_save());
        }
        let ui = self.clone();
        self.password.connect_changed(move |_| ui.queue_save());
        let ui = self.clone();
        self.port.connect_notify_local(Some("value"), move |_, _| ui.queue_save());
        for s in [&self.tls, &self.autorestart, &self.refresh_pw] {
            let ui = self.clone();
            s.connect_active_notify(move |_| ui.queue_save());
        }
        for c in [&self.codec, &self.decoder, &self.pix] {
            let ui = self.clone();
            c.connect_selected_notify(move |_| ui.queue_save());
        }

        let ui = self.clone();
        self.win.connect_close_request(move |_| {
            ui.save();
            ui.streamer.stop();
            glib::Propagation::Proceed
        });
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("io.github.IpCamWebcam").build();
    app.connect_activate(|app| {
        let ui = Ui::new(app);
        ui.win.present();
    });
    app.run()
}
