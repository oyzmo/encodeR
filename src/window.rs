//! Main window: header bar, queue, job editor, background work and dialogs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::checks::{self, CheckInput, Issue};
use crate::ffmpeg::capabilities::{self, Capabilities};
use crate::ffmpeg::command;
use crate::ffmpeg::estimate::{self, SizeRange};
use crate::ffmpeg::probe::{self, ProbeInfo};
use crate::ffmpeg::progress::Progress;
use crate::ffmpeg::runner::{Cancel, Helpers};
use crate::ffmpeg::vmaf::VmafResult;
use crate::presets;
use crate::queue::{self, CropState, EstimateState, Event, Job, JobState, VmafState};
use crate::settings::{EncodeSettings, Prefs, VideoCodec};
use crate::tracks::{self, PgsQuestion};
use crate::ui::editor::Editor;
use crate::ui::row::JobRow;
use crate::worker::{self, Outcome, Request, Tools, Update};

pub const APP_ID: &str = "no.oyzmo.EncodeR";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub enum Msg {
    Caps(Capabilities),
    Probed(u64, Result<Box<ProbeInfo>, String>),
    Analyzed(u64, Result<worker::Analysis, String>),
    Estimate(u64, String, Result<SizeRange, String>),
    Vmaf(u64, Result<VmafResult, String>),
    Progress(u64, Progress),
    Stage(u64, String),
    Finished(u64, Outcome),
}

pub struct State {
    pub jobs: Vec<Job>,
    pub prefs: Prefs,
    pub caps: Option<Capabilities>,
    pub tools: Tools,
    pub selected: Option<u64>,
    next_id: u64,
    next_batch: u64,
    pub running: Option<(u64, Cancel)>,
    pub queue_active: bool,
    est_gen: HashMap<u64, u64>,
    est_cancel: HashMap<u64, Cancel>,
    est_cache: HashMap<String, SizeRange>,
    pub vmaf_cancel: Option<(u64, Cancel)>,
    crop_cancel: HashMap<u64, Cancel>,
    pgs_session: HashMap<String, bool>,
    pgs_batch: HashMap<(u64, String), bool>,
}

impl State {
    pub fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn job_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    pub fn device(&self) -> String {
        self.caps
            .as_ref()
            .and_then(|c| c.device.clone())
            .or_else(|| self.prefs.device.clone())
            .unwrap_or_else(|| "/dev/dri/renderD128".into())
    }

    pub fn crop_on_gpu(&self) -> bool {
        self.caps.as_ref().is_some_and(|c| c.crop_on_gpu)
    }

    /// Video bitrate from the target-size maths for this job.
    pub fn video_kbps(&self, job: &Job) -> Option<u32> {
        let p = job.probe.as_ref()?;
        if !job.settings.bitrate_from_target() || job.settings.codec == VideoCodec::Copy {
            return None;
        }
        estimate::target_video_kbps(
            job.settings.target_gb,
            p.duration,
            job.selection.audio_bytes(p),
            job.selection.subtitle_bytes(p),
        )
    }

    pub fn request(&self, job: &Job) -> Option<Request> {
        let probe = job.probe.clone()?;
        Some(Request {
            output: queue::output_path(&job.input, &self.prefs, job.settings.container),
            device: self.device(),
            crop: job.active_crop(),
            crop_on_gpu: self.crop_on_gpu(),
            scan: job.scan_plan(),
            video_kbps: self.video_kbps(job),
            dv_preserve: job.dv_preserve,
            mkvpropedit: self.caps.as_ref().is_some_and(|c| c.mkvpropedit),
            vmaf_after: self.prefs.vmaf_after_encode
                && self.caps.as_ref().is_some_and(|c| c.libvmaf),
            settings: job.settings.clone(),
            selection: job.selection.clone(),
            probe,
        })
    }

    pub fn issues(&self, job: &Job) -> Vec<Issue> {
        let Some(p) = &job.probe else {
            return Vec::new();
        };
        let estimate = match &job.estimate {
            EstimateState::Ready(r) => Some(*r),
            _ => None,
        };
        let vmaf = match &job.vmaf {
            VmafState::Ready(r) => Some(r.mean),
            _ => None,
        };
        let mut v = checks::run(&CheckInput {
            probe: p,
            settings: &job.settings,
            selection: &job.selection,
            video_kbps: self.video_kbps(job),
            estimate,
            vmaf,
            dv_preserve: job.dv_preserve,
            crop: job.crop_enabled,
            active_crop: job.active_crop(),
            scan_detected: job.scan_detected,
            scan: job.scan_plan(),
            encoder_limits: self
                .caps
                .as_ref()
                .and_then(|c| c.codec(job.settings.codec).limits),
        });
        if let Some(reason) = self.codec_unavailable(&job.settings) {
            let alt = [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264]
                .into_iter()
                .find(|c| self.caps.as_ref().is_some_and(|k| k.codec(*c).available))
                .unwrap_or(VideoCodec::Copy);
            v.insert(
                0,
                Issue {
                    id: "nocodec",
                    severity: checks::Severity::Strong,
                    message: format!(
                        "{} can't be used on this system: {reason}",
                        job.settings.codec.label()
                    ),
                    fix: Some(checks::Fix::Codec(alt)),
                },
            );
        }
        v
    }

    /// Why the job's codec can't run here (after the capability check).
    pub fn codec_unavailable(&self, s: &EncodeSettings) -> Option<String> {
        let caps = self.caps.as_ref()?;
        let cc = caps.codec(s.codec);
        (!cc.available).then(|| cc.reason.unwrap_or_else(|| "not available".into()))
    }

    /// The exact command line for display.
    pub fn command_text(&self, job: &Job) -> String {
        let Some(req) = self.request(job) else {
            return String::new();
        };
        // The same job the encode will run, so the shown command can't drift.
        let j = req.job(&req.output, req.first_decode());
        let mut text = match command::build(&j) {
            Ok(a) => command::display_command(&self.tools.ffmpeg, &a),
            Err(e) => format!("# {e}"),
        };
        if req.dv_preserve {
            text = format!(
                "# Dolby Vision pipeline: ffmpeg | dovi_tool extract-rpu → encode to raw HEVC → dovi_tool inject-rpu → mux.\n# The final mux keeps these audio/subtitle settings:\n{text}"
            );
        }
        text
    }
}

pub struct App {
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    split: adw::NavigationSplitView,
    queue_list: gtk::ListBox,
    content_page: adw::NavigationPage,
    start_btn: gtk::Button,
    banner: adw::Banner,
    pub state: RefCell<State>,
    rows: RefCell<HashMap<u64, JobRow>>,
    pub editor: RefCell<Option<Rc<Editor>>>,
    pgs_queue: RefCell<VecDeque<(u64, PgsQuestion)>>,
    dialog_open: RefCell<bool>,
    /// Language groups whose subtitle list is expanded in the editor.
    pub subs_expanded: RefCell<HashSet<String>>,
    quit_pending: RefCell<bool>,
    helpers: Helpers,
    tx: async_channel::Sender<Msg>,
}

pub fn build(app: &adw::Application, files: Vec<PathBuf>) {
    let (tx, rx) = async_channel::unbounded::<Msg>();
    let prefs = Prefs::load();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("encodeR")
        .default_width(1180)
        .default_height(820)
        .width_request(360)
        .height_request(400)
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "encodeR",
        &format!("v{VERSION}"),
    )));
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add files…")
        .build();
    add_btn.set_action_name(Some("win.add-files"));
    let back_btn = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to the queue")
        .visible(false)
        .build();
    header.pack_start(&back_btn);
    header.pack_start(&add_btn);
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some("_Add Files…"), Some("win.add-files"));
    section.append(Some("Clear _Finished Jobs"), Some("win.clear-finished"));
    menu.append_section(None, &section);
    let section = gio::Menu::new();
    section.append(Some("_Preferences"), Some("win.preferences"));
    section.append(Some("_About encodeR"), Some("win.about"));
    menu.append_section(None, &section);
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .primary(true)
        .build();
    header.pack_end(&menu_btn);
    let start_btn = gtk::Button::builder()
        .label("Start")
        .tooltip_text("Encode the queued jobs one at a time")
        .sensitive(false)
        .build();
    start_btn.add_css_class("suggested-action");
    header.pack_end(&start_btn);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);

    let status = adw::StatusPage::builder()
        .icon_name("video-x-generic-symbolic")
        .title("Drop Videos Here")
        .description("Re-encode big files into a 2–5 GB library copy with the GPU.\nAudio and subtitle tracks are picked for you.")
        .build();
    let status_btn = gtk::Button::builder()
        .label("Add Files…")
        .halign(gtk::Align::Center)
        .action_name("win.add-files")
        .build();
    status_btn.add_css_class("pill");
    status_btn.add_css_class("suggested-action");
    status.set_child(Some(&status_btn));

    let queue_list = gtk::ListBox::new();
    queue_list.add_css_class("boxed-list");
    queue_list.set_selection_mode(gtk::SelectionMode::Single);
    queue_list.set_valign(gtk::Align::Start);
    let queue_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    queue_box.set_margin_top(12);
    queue_box.set_margin_bottom(12);
    queue_box.set_margin_start(12);
    queue_box.set_margin_end(12);
    queue_box.append(&queue_list);
    let queue_scroll = gtk::ScrolledWindow::builder()
        .child(&queue_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let content_page = adw::NavigationPage::builder()
        .title("Job")
        .tag("job")
        .build();
    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&adw::NavigationPage::new(&queue_scroll, "Queue")));
    split.set_content(Some(&content_page));
    split.set_min_sidebar_width(300.0);
    split.set_max_sidebar_width(420.0);
    split.set_sidebar_width_fraction(0.32);

    let stack = gtk::Stack::new();
    stack.add_named(&status, Some("empty"));
    stack.add_named(&split, Some("jobs"));
    stack.set_visible_child_name("empty");

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    let bp = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 760sp").expect("valid condition"),
    );
    bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(bp);

    {
        let (back, split2) = (back_btn.clone(), split.clone());
        let update = move |sv: &adw::NavigationSplitView| {
            back.set_visible(sv.is_collapsed() && sv.shows_content())
        };
        split2.connect_collapsed_notify(update.clone());
        split2.connect_show_content_notify(update);
        let split3 = split.clone();
        back_btn.connect_clicked(move |_| split3.set_show_content(false));
    }

    let app_rc = Rc::new(App {
        window: window.clone(),
        toasts,
        stack,
        split,
        queue_list: queue_list.clone(),
        content_page,
        start_btn: start_btn.clone(),
        banner: banner.clone(),
        state: RefCell::new(State {
            jobs: Vec::new(),
            prefs,
            caps: None,
            tools: Tools::default(),
            selected: None,
            next_id: 1,
            next_batch: 1,
            running: None,
            queue_active: false,
            est_gen: HashMap::new(),
            est_cancel: HashMap::new(),
            est_cache: HashMap::new(),
            vmaf_cancel: None,
            crop_cancel: HashMap::new(),
            pgs_session: HashMap::new(),
            pgs_batch: HashMap::new(),
        }),
        rows: RefCell::new(HashMap::new()),
        editor: RefCell::new(None),
        pgs_queue: RefCell::new(VecDeque::new()),
        dialog_open: RefCell::new(false),
        subs_expanded: RefCell::new(HashSet::new()),
        quit_pending: RefCell::new(false),
        helpers: Helpers::default(),
        tx,
    });

    install_actions(&app_rc);

    {
        let a = app_rc.clone();
        start_btn.connect_clicked(move |_| a.start_queue());
    }
    {
        let a = app_rc.clone();
        queue_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = a
                .rows
                .borrow()
                .iter()
                .find(|(_, r)| r.row == *row)
                .map(|(id, _)| *id);
            if let Some(id) = id {
                a.select(id);
            }
        });
    }
    {
        let a = app_rc.clone();
        banner.connect_button_clicked(move |_| a.show_caps_details());
    }

    // Drop files onto the window.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    {
        let a = app_rc.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            a.add_files(paths);
            true
        });
    }
    window.add_controller(drop);

    {
        let a = app_rc.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                a.handle(msg);
            }
        });
    }

    // Capability check in the background.
    {
        let tx = app_rc.tx.clone();
        let st = app_rc.state.borrow();
        let (ffmpeg, ffprobe, dev) = (
            st.tools.ffmpeg.clone(),
            st.tools.ffprobe.clone(),
            st.prefs.device.clone(),
        );
        std::thread::spawn(move || {
            let caps = capabilities::detect(&ffmpeg, &ffprobe, dev.as_deref());
            let _ = tx.send_blocking(Msg::Caps(caps));
        });
    }

    // Closing while encoding: ask, then stop ffmpeg cleanly before quitting.
    {
        let a = app_rc.clone();
        window.connect_close_request(move |w| {
            if a.state.borrow().running.is_none() || *a.quit_pending.borrow() {
                a.cancel_background();
                return glib::Propagation::Proceed;
            }
            let d = adw::AlertDialog::new(
                Some("Stop Encoding?"),
                Some("A job is still encoding. Quitting stops it and deletes the unfinished file."),
            );
            d.add_responses(&[("cancel", "Keep Encoding"), ("quit", "Stop and Quit")]);
            d.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
            d.set_close_response("cancel");
            let a2 = a.clone();
            d.connect_response(None, move |_, resp| {
                if resp == "quit" {
                    *a2.quit_pending.borrow_mut() = true;
                    a2.state.borrow_mut().queue_active = false;
                    if let Some((_, c)) = a2.state.borrow().running.as_ref() {
                        c.cancel();
                    }
                }
            });
            d.present(Some(w));
            glib::Propagation::Stop
        });
    }

    window.present();
    if !files.is_empty() {
        app_rc.add_files(files);
    }
}

fn install_actions(app: &Rc<App>) {
    let win = &app.window;
    let add = |name: &str, f: Box<dyn Fn()>| {
        let act = gio::SimpleAction::new(name, None);
        act.connect_activate(move |_, _| f());
        win.add_action(&act);
    };
    let a = app.clone();
    add("add-files", Box::new(move || a.pick_files()));
    let a = app.clone();
    add("clear-finished", Box::new(move || a.clear_finished()));
    let a = app.clone();
    add("preferences", Box::new(move || crate::ui::prefs::show(&a)));
    let a = app.clone();
    add("about", Box::new(move || a.show_about()));
    if let Some(gapp) = win.application() {
        gapp.set_accels_for_action("win.add-files", &["<Control>o"]);
        gapp.set_accels_for_action("win.preferences", &["<Control>comma"]);
        gapp.set_accels_for_action("window.close", &["<Control>w"]);
    }
}

impl App {
    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    pub fn save_prefs(&self) {
        self.state.borrow().prefs.save();
    }

    fn pick_files(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Videos"));
        filter.add_mime_type("video/*");
        for ext in ["mkv", "mp4", "m4v", "mov", "ts", "m2ts", "avi", "webm"] {
            filter.add_suffix(ext);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Add Videos")
            .modal(true)
            .filters(&filters)
            .build();
        let a = self.clone();
        dialog.open_multiple(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            if let Ok(model) = res {
                let paths: Vec<PathBuf> = (0..model.n_items())
                    .filter_map(|i| {
                        model
                            .item(i)
                            .and_downcast::<gio::File>()
                            .and_then(|f| f.path())
                    })
                    .collect();
                a.add_files(paths);
            }
        });
    }

    pub fn add_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut new_ids = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            let batch = st.next_batch;
            st.next_batch += 1;
            let preset = st.prefs.last_preset.clone();
            let settings = st.prefs.last_settings.clone();
            for p in paths {
                if st.jobs.iter().any(|j| {
                    j.input == p && !matches!(j.state, JobState::Done | JobState::Cancelled)
                }) {
                    continue;
                }
                let id = st.next_id;
                st.next_id += 1;
                st.jobs
                    .push(Job::new(id, batch, p, settings.clone(), preset.clone()));
                new_ids.push(id);
            }
        }
        for id in &new_ids {
            let row = JobRow::new();
            self.queue_list.append(&row.row);
            let a = self.clone();
            let id2 = *id;
            row.button.connect_clicked(move |_| a.cancel_or_remove(id2));
            self.rows.borrow_mut().insert(*id, row);
            self.update_row(*id);
            self.probe(*id);
        }
        self.stack.set_visible_child_name("jobs");
        if let Some(first) = new_ids.first()
            && self.state.borrow().selected.is_none()
        {
            self.select_row(*first);
        }
        self.update_start();
    }

    fn probe(&self, id: u64) {
        let (ffprobe, path) = {
            let st = self.state.borrow();
            let Some(j) = st.job(id) else { return };
            (st.tools.ffprobe.clone(), j.input.clone())
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = probe::run(&ffprobe, &path).map(Box::new);
            let _ = tx.send_blocking(Msg::Probed(id, r));
        });
    }

    fn select_row(&self, id: u64) {
        if let Some(r) = self.rows.borrow().get(&id) {
            self.queue_list.select_row(Some(&r.row));
        }
    }

    pub fn select(self: &Rc<Self>, id: u64) {
        self.state.borrow_mut().selected = Some(id);
        self.rebuild_editor();
        self.split.set_show_content(true);
    }

    /// Rebuild the editor for the selected job, keeping the scroll position.
    pub fn rebuild_editor(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().selected else {
            return;
        };
        let scroll = self
            .editor
            .borrow()
            .as_ref()
            .filter(|e| e.job == id)
            .map(|e| e.scroll_position());
        let editor = Editor::build(self, id);
        self.content_page.set_child(Some(&editor.root));
        let title = self
            .state
            .borrow()
            .job(id)
            .map(|j| j.file_name())
            .unwrap_or_default();
        self.content_page.set_title(&title);
        if let Some(v) = scroll {
            editor.restore_scroll(v);
        }
        *self.editor.borrow_mut() = Some(editor);
    }

    /// Refresh the parts of the editor that depend on derived data.
    pub fn refresh_editor(self: &Rc<Self>, id: u64) {
        let ed = self.editor.borrow().clone();
        if let Some(ed) = ed
            && ed.job == id
        {
            ed.refresh(self);
        }
    }

    pub fn update_row(&self, id: u64) {
        let st = self.state.borrow();
        let Some(job) = st.job(id) else { return };
        let worst = st
            .issues(job)
            .into_iter()
            .next()
            .map(|i| (i.severity, i.message));
        if let Some(r) = self.rows.borrow().get(&id) {
            r.update(job, worst);
        }
    }

    fn update_start(&self) {
        let st = self.state.borrow();
        let ready = queue::next_pending(&st.jobs).is_some();
        let running = st.running.is_some();
        self.start_btn.set_sensitive(ready && !running);
        self.start_btn
            .set_label(if running { "Encoding…" } else { "Start" });
    }

    /// Called after any edit to a job's settings or tracks.
    pub fn changed(self: &Rc<Self>, id: u64) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(j) = st.job(id).cloned() {
                st.prefs.last_settings = j.settings.clone();
                st.prefs.last_preset = j.preset.clone();
            }
        }
        self.save_prefs();
        self.schedule_estimate(id);
        self.update_row(id);
        self.refresh_editor(id);
    }

    pub fn edit(self: &Rc<Self>, id: u64, f: impl FnOnce(&mut Job)) {
        {
            let mut st = self.state.borrow_mut();
            let dovi_tool = st.caps.as_ref().is_some_and(|c| c.dovi_tool);
            let Some(j) = st.job_mut(id) else { return };
            if !j.state.editable() && j.state != JobState::Probing {
                return;
            }
            f(j);
            // Switching to AV1 or downscaling rules Dolby Vision out; a flag left on
            // would still send the job down the DV pipeline (and keep crop off).
            if j.dv_preserve && j.dv_blocker(dovi_tool).is_some() {
                j.dv_preserve = false;
            }
            // Settings edits invalidate the quality score, including one being measured.
            j.vmaf = VmafState::None;
            if st.vmaf_cancel.as_ref().is_some_and(|(v, _)| *v == id)
                && let Some((_, c)) = st.vmaf_cancel.take()
            {
                c.cancel();
            }
        }
        self.changed(id);
    }

    // ---- Message handling ----

    fn handle(self: &Rc<Self>, msg: Msg) {
        match msg {
            Msg::Caps(c) => self.on_caps(c),
            Msg::Probed(id, r) => self.on_probed(id, r),
            Msg::Analyzed(id, r) => {
                if let Some(j) = self.state.borrow_mut().job_mut(id) {
                    let (crop, scan) = match r {
                        Ok(a) => (a.crop, a.scan),
                        Err(e) => (Err(e), None),
                    };
                    j.crop = match crop {
                        Ok(Some(c)) => CropState::Found(c),
                        Ok(None) => CropState::None,
                        Err(e) => CropState::Failed(e),
                    };
                    j.scan_detected = scan;
                }
                // The scan plan changes the filters, so the estimate and command change too.
                self.schedule_estimate(id);
                self.update_row(id);
                self.refresh_editor(id);
                self.update_start();
                self.run_next();
            }
            Msg::Estimate(id, key, r) => {
                {
                    let mut st = self.state.borrow_mut();
                    if let Ok(v) = &r {
                        st.est_cache.insert(key.clone(), *v);
                    }
                    let current = st.job(id).map(estimate_key);
                    if current.as_deref() == Some(key.as_str()) {
                        let totals = st.job(id).and_then(|j| {
                            j.probe.as_ref().map(|p| {
                                (j.selection.audio_bytes(p), j.selection.subtitle_bytes(p))
                            })
                        });
                        if let Some(j) = st.job_mut(id) {
                            j.estimate = match (r, totals) {
                                (Ok(v), Some((a, s))) => {
                                    EstimateState::Ready(estimate::with_streams(v, a, s))
                                }
                                (Err(e), _) => EstimateState::Failed(e),
                                _ => EstimateState::None,
                            };
                        }
                    }
                }
                self.update_row(id);
                self.refresh_editor(id);
            }
            Msg::Vmaf(id, r) => {
                {
                    let mut st = self.state.borrow_mut();
                    if st.vmaf_cancel.as_ref().is_some_and(|(j, _)| *j == id) {
                        st.vmaf_cancel = None;
                    }
                    if let Some(j) = st.job_mut(id) {
                        j.vmaf = match r {
                            Ok(v) => VmafState::Ready(v),
                            Err(e) => VmafState::Failed(e),
                        };
                    }
                }
                self.update_row(id);
                self.refresh_editor(id);
            }
            Msg::Progress(id, p) => {
                if let Some(j) = self.state.borrow_mut().job_mut(id) {
                    j.progress = Some(p);
                }
                self.update_row(id);
                if let Some(ed) = self.editor.borrow().as_ref().filter(|e| e.job == id) {
                    ed.update_progress(&self.state.borrow());
                }
            }
            Msg::Stage(id, s) => {
                if let Some(j) = self.state.borrow_mut().job_mut(id) {
                    j.stage = Some(s);
                }
                self.update_row(id);
            }
            Msg::Finished(id, o) => self.on_finished(id, o),
        }
    }

    fn on_caps(self: &Rc<Self>, caps: Capabilities) {
        let problem = if !caps.ffmpeg {
            Some((
                "ffmpeg is missing",
                "Install the RPM Fusion build of ffmpeg:\nsudo dnf install ffmpeg --allowerasing"
                    .to_string(),
            ))
        } else if !caps.ffprobe {
            Some((
                "ffprobe is missing",
                "ffprobe comes with ffmpeg. Install the RPM Fusion build of ffmpeg.".to_string(),
            ))
        } else if !caps.missing_filters.is_empty() {
            Some((
                "ffmpeg is missing filters",
                format!(
                    "This ffmpeg lacks: {}. Black-bar and interlace detection, deinterlacing or inverse telecine may fail. Use the RPM Fusion ffmpeg (or a build with --enable-gpl).",
                    caps.missing_filters.join(", ")
                ),
            ))
        } else if !caps.any_hw_encoder() {
            let reasons: Vec<String> = [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1]
                .iter()
                .map(|c| {
                    format!(
                        "{}: {}",
                        c.label(),
                        caps.codec(*c).reason.unwrap_or_default()
                    )
                })
                .collect();
            Some((
                "No VA-API encoder works",
                format!(
                    "Hardware encoding failed on {}. Only “Copy (remux only)” is available.\n\n{}\n\nCheck that mesa-va-drivers-freeworld is installed and that you're in the “render” group.",
                    caps.device.as_deref().unwrap_or("any device"),
                    reasons.join("\n")
                ),
            ))
        } else {
            None
        };
        if let Some((title, _)) = &problem {
            self.banner.set_title(title);
            self.banner.set_button_label(Some("Details"));
            self.banner.set_revealed(true);
        }
        self.state.borrow_mut().caps = Some(caps);
        if let Some((title, body)) = problem {
            let d = adw::AlertDialog::new(Some(title), Some(&body));
            d.add_response("ok", "OK");
            d.present(Some(&self.window));
        }
        let ids: Vec<u64> = self.state.borrow().jobs.iter().map(|j| j.id).collect();
        for id in ids {
            self.schedule_estimate(id);
            self.update_row(id);
        }
        self.rebuild_editor();
    }

    fn show_caps_details(&self) {
        let st = self.state.borrow();
        let Some(c) = &st.caps else { return };
        let mut lines = vec![
            c.ffmpeg_version.clone(),
            format!("Device: {}", c.device.as_deref().unwrap_or("none")),
        ];
        for codec in [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1] {
            let cc = c.codec(codec);
            lines.push(if cc.available {
                format!(
                    "{}: OK{}",
                    codec.label(),
                    if cc.ten_bit { ", 10-bit" } else { "" }
                )
            } else {
                format!("{}: {}", codec.label(), cc.reason.unwrap_or_default())
            });
        }
        let d = adw::AlertDialog::new(Some("Hardware Encoding"), Some(&lines.join("\n")));
        d.add_response("ok", "OK");
        d.present(Some(&self.window));
    }

    fn on_probed(self: &Rc<Self>, id: u64, r: Result<Box<ProbeInfo>, String>) {
        let mut questions = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            let prefs = st.prefs.preferred_languages.clone();
            let Some(j) = st.job_mut(id) else { return };
            match r {
                Ok(p) => {
                    let (sel, groups, qs) = tracks::auto_select(&p, &prefs, &j.settings);
                    j.selection = sel;
                    j.groups = groups;
                    j.pgs_pending = qs.clone();
                    j.probe = Some(*p);
                    j.apply(Event::ProbeOk);
                    questions = qs;
                }
                Err(e) => {
                    j.error = Some(e.clone());
                    j.apply(Event::ProbeFailed);
                    drop(st);
                    self.toast(&format!(
                        "Couldn't read {}",
                        self.state
                            .borrow()
                            .job(id)
                            .map(|j| j.file_name())
                            .unwrap_or_default()
                    ));
                }
            }
        }
        for q in questions {
            self.pgs_queue.borrow_mut().push_back((id, q));
        }
        self.detect_crop(id);
        self.schedule_estimate(id);
        self.update_row(id);
        if self.state.borrow().selected == Some(id) {
            self.rebuild_editor();
        }
        self.update_start();
        self.process_pgs();
        self.run_next();
    }

    // ---- PGS subtitle questions ----

    fn resolve_pgs(self: &Rc<Self>, batch: u64, family: &str, keep: bool) {
        let ids: Vec<u64> = {
            let mut st = self.state.borrow_mut();
            let mut ids = Vec::new();
            for j in st.jobs.iter_mut().filter(|j| j.batch == batch) {
                if let Some(pos) = j.pgs_pending.iter().position(|q| q.family == family) {
                    let q = j.pgs_pending.remove(pos);
                    j.selection.set_pgs(&q, keep);
                    ids.push(j.id);
                }
            }
            ids
        };
        self.pgs_queue
            .borrow_mut()
            .retain(|(id, q)| !(ids.contains(id) && q.family == family));
        for id in ids {
            self.changed(id);
            if self.state.borrow().selected == Some(id) {
                self.rebuild_editor();
            }
        }
        self.update_start();
        self.run_next();
    }

    fn process_pgs(self: &Rc<Self>) {
        if *self.dialog_open.borrow() {
            return;
        }
        loop {
            let Some((id, q)) = self.pgs_queue.borrow_mut().pop_front() else {
                return;
            };
            let batch = match self.state.borrow().job(id) {
                Some(j) if j.pgs_pending.iter().any(|p| p.family == q.family) => j.batch,
                _ => continue,
            };
            let decided = {
                let st = self.state.borrow();
                st.pgs_batch
                    .get(&(batch, q.family.clone()))
                    .or_else(|| st.pgs_session.get(&q.family))
                    .copied()
            };
            if let Some(keep) = decided {
                self.resolve_pgs(batch, &q.family, keep);
                continue;
            }
            self.ask_pgs(batch, q);
            return;
        }
    }

    fn ask_pgs(self: &Rc<Self>, batch: u64, q: PgsQuestion) {
        *self.dialog_open.borrow_mut() = true;
        let d = adw::AlertDialog::new(
            Some(&format!("Keep PGS Subtitles for {}?", q.name)),
            Some(&format!(
                "Only PGS subtitles available for {}. PGS may force Plex to transcode (burn in) when these subtitles are turned on. Keep them?",
                q.name
            )),
        );
        d.add_responses(&[("drop", "Drop"), ("keep", "Keep")]);
        d.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("keep"));
        d.set_close_response("drop");
        let remember = gtk::CheckButton::with_label("Remember for this session");
        remember.set_halign(gtk::Align::Center);
        d.set_extra_child(Some(&remember));
        let a = self.clone();
        d.connect_response(None, move |_, resp| {
            let keep = resp == "keep";
            {
                let mut st = a.state.borrow_mut();
                st.pgs_batch.insert((batch, q.family.clone()), keep);
                if remember.is_active() {
                    st.pgs_session.insert(q.family.clone(), keep);
                }
            }
            *a.dialog_open.borrow_mut() = false;
            a.resolve_pgs(batch, &q.family, keep);
            a.process_pgs();
        });
        d.present(Some(&self.window));
    }

    // ---- Background helpers ----

    fn detect_crop(&self, id: u64) {
        let (tools, probe) = {
            let mut st = self.state.borrow_mut();
            let tools = st.tools.clone();
            let Some(j) = st.job_mut(id) else { return };
            let Some(p) = j.probe.clone() else { return };
            j.crop = CropState::Detecting;
            (tools, p)
        };
        let cancel = Cancel::default();
        self.state
            .borrow_mut()
            .crop_cancel
            .insert(id, cancel.clone());
        let tx = self.tx.clone();
        let guard = self.helpers.enter();
        std::thread::spawn(move || {
            let _guard = guard;
            let r = worker::analyze(&tools, &probe, &cancel);
            let _ = tx.send_blocking(Msg::Analyzed(id, r));
        });
    }

    /// Debounced (500 ms) size estimate.
    pub fn schedule_estimate(self: &Rc<Self>, id: u64) {
        let generation = {
            let mut st = self.state.borrow_mut();
            let g = st.est_gen.entry(id).or_insert(0);
            *g += 1;
            *g
        };
        let a = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if a.state.borrow().est_gen.get(&id) == Some(&generation) {
                a.start_estimate(id);
            }
        });
    }

    fn start_estimate(self: &Rc<Self>, id: u64) {
        let mut st = self.state.borrow_mut();
        if let Some(c) = st.est_cancel.remove(&id) {
            c.cancel();
        }
        let Some(job) = st.job(id).cloned() else {
            return;
        };
        let Some(p) = job.probe.as_ref() else { return };
        let (audio, subs) = (
            job.selection.audio_bytes(p),
            job.selection.subtitle_bytes(p),
        );
        let s = &job.settings;
        let rc = s.effective_rate_control();
        let state = if s.codec == VideoCodec::Copy {
            let size = estimate::copy_size(p, &job.selection);
            EstimateState::Ready(SizeRange { lo: size, hi: size })
        } else if rc.uses_bitrate() && rc != crate::settings::RateControl::Qvbr {
            let kbps = if s.bitrate_from_target() {
                st.video_kbps(&job)
            } else {
                Some(s.bitrate_kbps)
            };
            match kbps {
                Some(k) => {
                    EstimateState::Ready(estimate::bitrate_range(k, p.duration, audio, subs, false))
                }
                None => EstimateState::Failed("The audio alone is bigger than the target".into()),
            }
        } else {
            let key = estimate_key(&job);
            if let Some(v) = st.est_cache.get(&key) {
                EstimateState::Ready(estimate::with_streams(*v, audio, subs))
            } else if !st.caps.as_ref().is_some_and(|c| c.codec(s.codec).available) {
                EstimateState::Failed(if st.caps.is_none() {
                    "Checking the GPU…".into()
                } else {
                    "Needs a working VA-API encoder".into()
                })
            } else if let Some(req) = st.request(&job) {
                let cancel = Cancel::default();
                st.est_cancel.insert(id, cancel.clone());
                let tools = st.tools.clone();
                let tx = self.tx.clone();
                let guard = self.helpers.enter();
                std::thread::spawn(move || {
                    let _guard = guard;
                    let r = worker::estimate_video(&tools, &req, &cancel);
                    if !cancel.is_cancelled() {
                        let _ = tx.send_blocking(Msg::Estimate(id, key, r));
                    }
                });
                EstimateState::Running
            } else {
                EstimateState::None
            }
        };
        if let Some(j) = st.job_mut(id) {
            j.estimate = state;
        }
        drop(st);
        self.update_row(id);
        self.refresh_editor(id);
    }

    pub fn start_vmaf(self: &Rc<Self>, id: u64) {
        let stopped = self.stop_vmaf();
        if let Some(prev) = stopped.filter(|p| *p != id) {
            self.update_row(prev);
        }
        let mut st = self.state.borrow_mut();
        let Some(job) = st.job(id).cloned() else {
            return;
        };
        let Some(req) = st.request(&job) else { return };
        let cancel = Cancel::default();
        st.vmaf_cancel = Some((id, cancel.clone()));
        let tools = st.tools.clone();
        if let Some(j) = st.job_mut(id) {
            j.vmaf = VmafState::Running;
        }
        drop(st);
        let tx = self.tx.clone();
        let guard = self.helpers.enter();
        std::thread::spawn(move || {
            let _guard = guard;
            let r = worker::vmaf_samples(&tools, &req, &cancel);
            // A cancelled run's job state was already reset by `stop_vmaf`.
            if !cancel.is_cancelled() {
                let _ = tx.send_blocking(Msg::Vmaf(id, r));
            }
        });
        self.refresh_editor(id);
    }

    pub fn cancel_vmaf(self: &Rc<Self>) {
        if let Some(id) = self.stop_vmaf() {
            self.update_row(id);
            self.refresh_editor(id);
        }
    }

    /// Stop the running VMAF check and mark its job as not scored; returns that job.
    fn stop_vmaf(&self) -> Option<u64> {
        let mut st = self.state.borrow_mut();
        let (id, c) = st.vmaf_cancel.take()?;
        c.cancel();
        if let Some(j) = st.job_mut(id) {
            j.vmaf = VmafState::None;
        }
        Some(id)
    }

    // ---- Queue ----

    /// Cancel estimates, crop detection and VMAF (on quit), and give their
    /// threads a moment to kill ffmpeg and delete sample files before exiting.
    fn cancel_background(&self) {
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            for c in st.est_cancel.values().chain(st.crop_cancel.values()) {
                c.cancel();
            }
            if let Some((_, c)) = st.vmaf_cancel.take() {
                c.cancel();
            }
        }
        // Helpers poll the flag every 50 ms; this only blocks if one is stuck.
        self.helpers.wait_idle(Duration::from_secs(2));
    }

    fn start_queue(self: &Rc<Self>) {
        // Warn once about strong issues or a too-low target bitrate before starting.
        let warnings: Vec<String> = {
            let st = self.state.borrow();
            st.jobs
                .iter()
                .filter(|j| j.state == JobState::Pending)
                .filter_map(|j| {
                    let bad: Vec<String> = st
                        .issues(j)
                        .into_iter()
                        .filter(|i| i.severity == checks::Severity::Strong || i.id == "lowtarget")
                        .map(|i| i.message)
                        .collect();
                    (!bad.is_empty()).then(|| format!("{}: {}", j.file_name(), bad.join(" ")))
                })
                .collect()
        };
        if warnings.is_empty() {
            self.state.borrow_mut().queue_active = true;
            self.run_next();
            return;
        }
        let d = adw::AlertDialog::new(Some("Start Anyway?"), Some(&warnings.join("\n\n")));
        d.add_responses(&[("cancel", "Cancel"), ("start", "Start Anyway")]);
        d.set_response_appearance("start", adw::ResponseAppearance::Suggested);
        d.set_close_response("cancel");
        let a = self.clone();
        d.connect_response(None, move |_, resp| {
            if resp == "start" {
                a.state.borrow_mut().queue_active = true;
                a.run_next();
            }
        });
        d.present(Some(&self.window));
    }

    fn run_next(self: &Rc<Self>) {
        let (id, output) = {
            let st = self.state.borrow();
            if !st.queue_active || st.running.is_some() {
                return;
            }
            let Some(id) = queue::next_pending(&st.jobs) else {
                // Jobs still probing, waiting for a PGS answer or for analysis start
                // when they're ready (`on_probed`, `resolve_pgs`, `Msg::Analyzed`).
                let waiting = st
                    .jobs
                    .iter()
                    .any(|j| matches!(j.state, JobState::Pending | JobState::Probing));
                drop(st);
                if !waiting {
                    self.state.borrow_mut().queue_active = false;
                }
                self.update_start();
                return;
            };
            let j = st.job(id).expect("pending job exists");
            if let Some(reason) = st.codec_unavailable(&j.settings) {
                let msg = format!("{} isn't available: {reason}", j.settings.codec.label());
                drop(st);
                if let Some(j) = self.state.borrow_mut().job_mut(id) {
                    j.apply(Event::Start);
                    j.apply(Event::Failed);
                    j.error = Some(msg);
                }
                self.update_row(id);
                if self.state.borrow().selected == Some(id) {
                    self.rebuild_editor();
                }
                self.run_next();
                return;
            }
            (
                id,
                queue::output_path(&j.input, &st.prefs, j.settings.container),
            )
        };
        if output.exists() {
            let d = adw::AlertDialog::new(
                Some("Replace Existing File?"),
                Some(&format!(
                    "“{}” already exists. Replacing it will overwrite its contents.",
                    output.display()
                )),
            );
            d.add_responses(&[("skip", "Skip Job"), ("replace", "Replace")]);
            d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
            d.set_close_response("skip");
            let a = self.clone();
            d.connect_response(None, move |_, resp| {
                if resp == "replace" {
                    a.launch(id);
                } else {
                    if let Some(j) = a.state.borrow_mut().job_mut(id) {
                        j.apply(Event::Cancel);
                        j.error = Some("Output file already exists".into());
                    }
                    a.update_row(id);
                    a.run_next();
                }
            });
            d.present(Some(&self.window));
        } else {
            self.launch(id);
        }
    }

    fn launch(self: &Rc<Self>, id: u64) {
        let (req, tools) = {
            let mut st = self.state.borrow_mut();
            let Some(job) = st.job(id).cloned() else {
                return;
            };
            let Some(req) = st.request(&job) else { return };
            let tools = st.tools.clone();
            let cancel = Cancel::default();
            st.running = Some((id, cancel));
            let j = st.job_mut(id).expect("job exists");
            j.apply(Event::Start);
            j.progress = None;
            j.stage = Some("Starting".into());
            j.error = None;
            j.stderr_tail.clear();
            j.report.clear();
            j.output = Some(req.output.clone());
            (req, tools)
        };
        let cancel = self
            .state
            .borrow()
            .running
            .as_ref()
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = worker::encode(&tools, &req, &cancel, |u| {
                let _ = tx.send_blocking(match u {
                    Update::Progress(p) => Msg::Progress(id, p),
                    Update::Stage(s) => Msg::Stage(id, s),
                });
            });
            let _ = tx.send_blocking(Msg::Finished(id, out));
        });
        self.update_row(id);
        self.update_start();
        if self.state.borrow().selected == Some(id) {
            self.rebuild_editor();
        }
    }

    fn on_finished(self: &Rc<Self>, id: u64, o: Outcome) {
        let name;
        {
            let mut st = self.state.borrow_mut();
            st.running = None;
            let Some(j) = st.job_mut(id) else { return };
            name = j.file_name();
            j.stage = None;
            j.decode_used = o.decode;
            j.stderr_tail = o.stderr_tail;
            j.report = o.report;
            if o.cancelled {
                j.apply(Event::Cancel);
            } else if o.success {
                j.apply(Event::Succeeded);
            } else {
                j.error = o.error;
                j.apply(Event::Failed);
            }
        }
        if *self.quit_pending.borrow() {
            self.window.close();
            return;
        }
        let state = self.state.borrow().job(id).map(|j| j.state);
        match state {
            Some(JobState::Failed) => self.toast(&format!("Encoding failed: {name}")),
            Some(JobState::Done) => self.toast(&format!("Finished: {name}")),
            Some(JobState::Cancelled) => self.toast(&format!("Cancelled: {name}")),
            _ => {}
        }
        self.update_row(id);
        if self.state.borrow().selected == Some(id) {
            self.rebuild_editor();
        }
        self.update_start();
        self.run_next();
    }

    fn cancel_or_remove(self: &Rc<Self>, id: u64) {
        let running = self
            .state
            .borrow()
            .running
            .as_ref()
            .filter(|(r, _)| *r == id)
            .map(|(_, c)| c.clone());
        if let Some(c) = running {
            c.cancel();
            if let Some(j) = self.state.borrow_mut().job_mut(id) {
                j.stage = Some("Stopping…".into());
            }
            self.update_row(id);
            return;
        }
        self.remove_job(id);
    }

    fn remove_job(self: &Rc<Self>, id: u64) {
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            for map in [&mut st.est_cancel, &mut st.crop_cancel] {
                if let Some(c) = map.remove(&id) {
                    c.cancel();
                }
            }
            if st.vmaf_cancel.as_ref().is_some_and(|(j, _)| *j == id)
                && let Some((_, c)) = st.vmaf_cancel.take()
            {
                c.cancel();
            }
            st.jobs.retain(|j| j.id != id);
            if st.selected == Some(id) {
                st.selected = None;
            }
        }
        if let Some(r) = self.rows.borrow_mut().remove(&id) {
            self.queue_list.remove(&r.row);
        }
        self.pgs_queue.borrow_mut().retain(|(j, _)| *j != id);
        let next = self.state.borrow().jobs.first().map(|j| j.id);
        match next {
            Some(n) if self.state.borrow().selected.is_none() => self.select_row(n),
            None => {
                *self.editor.borrow_mut() = None;
                self.content_page.set_child(None::<&gtk::Widget>);
                self.stack.set_visible_child_name("empty");
            }
            _ => {}
        }
        self.update_start();
    }

    fn clear_finished(self: &Rc<Self>) {
        let ids: Vec<u64> = self
            .state
            .borrow()
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Done)
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.remove_job(id);
        }
    }

    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(j) = self.state.borrow_mut().job_mut(id) {
            j.apply(Event::Retry);
            j.error = None;
        }
        self.update_row(id);
        self.update_start();
        self.rebuild_editor();
    }

    /// Copy this job's settings to every other job that hasn't started.
    pub fn apply_to_all(self: &Rc<Self>, id: u64) {
        let n = {
            let mut st = self.state.borrow_mut();
            let Some(src) = st.job(id).cloned() else {
                return;
            };
            let mut n = 0;
            for j in st
                .jobs
                .iter_mut()
                .filter(|j| j.id != id && j.state == JobState::Pending)
            {
                j.settings = src.settings.clone();
                j.preset = src.preset.clone();
                if let Some(p) = &j.probe {
                    j.selection.reset_actions(p, &j.settings);
                }
                n += 1;
            }
            n
        };
        let ids: Vec<u64> = self.state.borrow().jobs.iter().map(|j| j.id).collect();
        for j in ids {
            self.schedule_estimate(j);
            self.update_row(j);
        }
        self.toast(&format!(
            "Applied to {n} other job{}",
            if n == 1 { "" } else { "s" }
        ));
    }

    pub fn apply_preset(self: &Rc<Self>, id: u64, name: &str) {
        let Some(p) = presets::find(&self.state.borrow().prefs.user_presets, name) else {
            return;
        };
        self.edit(id, |j| {
            j.settings = p.settings.clone();
            j.preset = p.name.clone();
            if let Some(pr) = &j.probe {
                j.selection.reset_actions(pr, &j.settings);
            }
        });
        self.rebuild_editor();
    }

    pub fn save_preset_dialog(self: &Rc<Self>, id: u64) {
        let d = adw::AlertDialog::new(
            Some("Save Preset"),
            Some("Saves the current video, size and audio settings."),
        );
        let entry = gtk::Entry::builder()
            .placeholder_text("Preset name")
            .activates_default(true)
            .build();
        d.set_extra_child(Some(&entry));
        d.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("save"));
        d.set_close_response("cancel");
        let a = self.clone();
        d.connect_response(None, move |_, resp| {
            if resp != "save" {
                return;
            }
            let name = entry.text().to_string();
            let result = {
                let mut st = a.state.borrow_mut();
                let settings: EncodeSettings =
                    st.job(id).map(|j| j.settings.clone()).unwrap_or_default();
                let r = presets::save(&mut st.prefs.user_presets, &name, &settings);
                if r.is_ok()
                    && let Some(j) = st.job_mut(id)
                {
                    j.preset = name.trim().to_string();
                }
                r
            };
            match result {
                Ok(()) => {
                    a.changed(id);
                    a.rebuild_editor();
                    a.toast("Preset saved");
                }
                Err(e) => a.toast(&e),
            }
        });
        d.present(Some(&self.window));
    }

    pub fn apply_fix(self: &Rc<Self>, id: u64, fix: checks::Fix) {
        self.edit(id, |j| {
            if let Some(p) = j.probe.clone() {
                checks::apply_fix(fix, &p, &mut j.settings, &mut j.selection);
            }
        });
        self.rebuild_editor();
    }

    pub fn crop_preview(self: &Rc<Self>, id: u64) {
        crate::ui::preview::show(self, id);
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("encodeR")
            .application_icon(APP_ID)
            .version(VERSION)
            .developer_name("oyzmo")
            .website("https://oyzmo.no")
            .license_type(gtk::License::Gpl30)
            .comments("Shrink large videos into library copies with AMD VA-API hardware encoding, tuned for direct play on Apple TV.")
            .build();
        about.present(Some(&self.window));
    }

    pub fn reprobe_all_tracks(self: &Rc<Self>) {
        // Preferred languages changed: re-run auto-selection on jobs that haven't started.
        let ids: Vec<u64> = {
            let mut st = self.state.borrow_mut();
            let prefs = st.prefs.preferred_languages.clone();
            let mut ids = Vec::new();
            for j in st.jobs.iter_mut().filter(|j| j.state == JobState::Pending) {
                if let Some(p) = &j.probe {
                    let (sel, groups, qs) = tracks::auto_select(p, &prefs, &j.settings);
                    j.selection = sel;
                    j.groups = groups;
                    j.pgs_pending = qs.clone();
                    ids.push((j.id, qs));
                }
            }
            ids.into_iter()
                .map(|(id, qs)| {
                    for q in qs {
                        self.pgs_queue.borrow_mut().push_back((id, q));
                    }
                    id
                })
                .collect()
        };
        for id in ids {
            self.schedule_estimate(id);
            self.update_row(id);
        }
        self.rebuild_editor();
        self.process_pgs();
    }

    pub fn cancel_running(self: &Rc<Self>, id: u64) {
        self.cancel_or_remove(id);
    }
}

/// Cache key for sample estimates: file + settings + crop + deinterlace plan.
pub fn estimate_key(j: &Job) -> String {
    format!(
        "{}|{}|{:?}|{:?}",
        j.input.display(),
        j.settings.cache_key(),
        j.active_crop(),
        j.scan_plan()
    )
}
