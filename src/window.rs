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
use crate::ffmpeg::runner;
use crate::ffmpeg::runner::{Cancel, Helpers};
use crate::ffmpeg::vmaf::VmafResult;
use crate::presets;
use crate::queue::{self, CropState, EstimateState, Event, Job, JobState, VmafState};
use crate::settings::{EncodeSettings, Prefs, VideoCodec, cache_dir};
use crate::subs::{self, ExternalSub, JobMode, SubsTarget};
use crate::tracks::{self, PgsQuestion};
use crate::ui::editor::Editor;
use crate::ui::row::JobRow;
use crate::worker::{self, Outcome, Request, Tools, Update};

pub const APP_ID: &str = "no.oyzmo.EncodeR";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub enum Msg {
    Caps(Capabilities),
    /// Probe result, plus the sidecar subtitle files found next to the video.
    Probed(u64, Result<Box<ProbeInfo>, String>, Vec<ExternalSub>),
    Analyzed(u64, Result<worker::Analysis, String>),
    Estimate(u64, String, Result<SizeRange, String>),
    /// The estimate key the score was measured with, checked on arrival.
    Vmaf(u64, String, Result<VmafResult, String>),
    Progress(u64, Progress),
    Stage(u64, String),
    Finished(u64, Outcome),
    /// Post-encode VMAF line for a finished job.
    PostVmaf(u64, String),
    /// The one-click alass install finished: the program path, or why not.
    AlassInstalled(Result<String, String>),
    /// Dropped or picked files with their zips unpacked, the zips that failed,
    /// and what to do with the files.
    Unzipped(Vec<PathBuf>, Vec<String>, AfterUnzip),
}

/// What to do with files once their zips are unpacked.
pub enum AfterUnzip {
    /// Queue them (`add_files_as`).
    Add(JobMode),
    /// Give the subtitles to this job.
    Attach(u64),
}

/// Replace each zip in `paths` with its SRT files (see `subs::unpack_zip`).
/// Each zip gets its own cache folder named after its path, size and date, so
/// the same zip added twice gives the same files. Blocking.
fn unzip_all(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<String>) {
    use std::hash::{Hash, Hasher};
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        if !subs::is_zip(&p) {
            out.push(p);
            continue;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        p.hash(&mut h);
        if let Ok(m) = std::fs::metadata(&p) {
            m.len().hash(&mut h);
            m.modified().ok().hash(&mut h);
        }
        let dir = cache_dir().join(format!("unzip-{}-{:016x}", std::process::id(), h.finish()));
        match subs::unpack_zip(&p, &dir) {
            Ok(srts) => out.extend(srts),
            Err(e) => errors.push(e),
        }
    }
    (out, errors)
}

/// Report line shown while the post-encode VMAF runs; replaced by the result.
const POST_VMAF_PENDING: &str = "VMAF: scoring the output in the background…";

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
    /// Job whose "Replace Existing File?" dialog is open: nothing else starts
    /// until it's answered, or the same job could be launched twice.
    replace_pending: Option<u64>,
    est_gen: HashMap<u64, u64>,
    est_cancel: HashMap<u64, Cancel>,
    est_cache: HashMap<String, SizeRange>,
    pub vmaf_cancel: Option<(u64, Cancel)>,
    crop_cancel: HashMap<u64, Cancel>,
    /// Post-encode VMAF runs, which never hold up the queue.
    post_vmaf: HashMap<u64, Cancel>,
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
            job.selection.subtitle_bytes(p) + job.external_bytes(),
        )
    }

    /// Where job `id`'s poster goes: next to the file it writes, or next to
    /// the video when a fix-tracks job leaves it in place.
    pub fn poster_for(&self, job: &Job) -> std::path::PathBuf {
        let video = if job.writes_output() {
            self.output_for(job)
        } else {
            job.input.clone()
        };
        crate::cover::poster_path(&video)
    }

    /// Output path for a job: never its own input, another job's input, or the
    /// output of a job earlier in the queue.
    pub fn output_for(&self, job: &Job) -> std::path::PathBuf {
        // In-place fix-tracks jobs write next to (or into) their own input.
        if let JobMode::Fix(t @ (SubsTarget::Sidecar | SubsTarget::Embed)) = job.mode {
            let first = (t == SubsTarget::Sidecar)
                .then(|| subs::sidecar_outputs(&job.input, &job.externals).ok())
                .flatten()
                .and_then(|v| v.into_iter().next());
            return first.unwrap_or_else(|| job.input.clone());
        }
        let mut taken: Vec<std::path::PathBuf> =
            self.jobs.iter().map(|j| j.input.clone()).collect();
        for j in &self.jobs {
            if j.id == job.id {
                break;
            }
            let written = matches!(j.state, JobState::Running | JobState::Done)
                .then(|| j.output.clone())
                .flatten();
            taken.push(written.unwrap_or_else(|| {
                queue::output_path_avoiding(&j.input, &self.prefs, j.output_container(), &taken)
            }));
        }
        queue::output_path_avoiding(&job.input, &self.prefs, job.output_container(), &taken)
    }

    pub fn request(&self, job: &Job) -> Option<Request> {
        let probe = job.probe.clone()?;
        Some(Request {
            output: self.output_for(job),
            device: self.device(),
            crop: job.active_crop(),
            crop_on_gpu: self.crop_on_gpu(),
            scan: job.scan_plan(),
            video_kbps: self.video_kbps(job),
            dv_preserve: job.dv_preserve,
            mkvpropedit: self.caps.as_ref().is_some_and(|c| c.mkvpropedit),
            externals: job.externals.clone(),
            mode: job.mode,
            alass: self.caps.as_ref().and_then(|c| c.alass.clone()),
            cover: job.cover.clone(),
            covers: Vec::new(),
            settings: job.settings.clone(),
            selection: job.selection.clone(),
            probe,
        })
    }

    pub fn issues(&self, job: &Job) -> Vec<Issue> {
        let Some(p) = &job.probe else {
            return Vec::new();
        };
        let alass = self.caps.as_ref().is_none_or(|c| c.alass.is_some());
        let mut sub_issues = checks::external_issues(
            &job.input,
            &job.settings,
            job.mode,
            &job.externals,
            job.fix_work(),
            alass,
        );
        checks::offer_alass_install(&mut sub_issues, crate::alass::installable());
        if job.mode.fix_only() {
            return sub_issues;
        }
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
                    alt_fix: None,
                },
            );
        }
        v.extend(sub_issues);
        v.sort_by_key(|i| std::cmp::Reverse(i.severity));
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
        let Some(mut req) = self.request(job) else {
            return String::new();
        };
        // Cover pictures are extracted and converted to a temp folder at run time.
        let temp = std::path::Path::new("<temp>");
        let poster_note = |video: &std::path::Path| {
            if job.cover.wants_poster(&req.probe) {
                format!(
                    "\n# saves the poster {}",
                    crate::cover::poster_path(video).display()
                )
            } else {
                String::new()
            }
        };
        match job.mode {
            JobMode::Fix(SubsTarget::Sidecar) => {
                let mut text = if job.externals.iter().any(|e| e.selected) {
                    match subs::sidecar_outputs(&job.input, &job.externals) {
                        Ok(v) => v
                            .iter()
                            .map(|p| format!("# saves {}", p.display()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        Err(e) => format!("# {e}"),
                    }
                } else {
                    String::new()
                };
                text.push_str(&poster_note(&job.input));
                return text.trim_start().to_string();
            }
            JobMode::Fix(t) => {
                if !job.remuxes() && job.fix_work().poster {
                    return poster_note(&job.input).trim_start().to_string();
                }
                let c =
                    subs::fix_container(&job.input, t).unwrap_or(crate::settings::Container::Mkv);
                let dest = if t == SubsTarget::Embed {
                    job.input.clone()
                } else {
                    req.output.clone()
                };
                let part = worker::partial_path(&dest);
                let delays = subs::audio_delays(&job.selection);
                let plan = crate::cover::plan(&req.probe, &job.cover, c, true, temp);
                return match command::build_fix(
                    &req.probe,
                    &job.externals,
                    &delays,
                    &plan,
                    c,
                    &part,
                ) {
                    Ok(a) => format!(
                        "{}\n# then the checked file {} {}{}",
                        command::display_command(&self.tools.ffmpeg, &a),
                        if t == SubsTarget::Embed {
                            "replaces"
                        } else {
                            "is renamed to"
                        },
                        dest.display(),
                        poster_note(&dest)
                    ),
                    Err(e) => format!("# {e}"),
                };
            }
            JobMode::Encode => {}
        }
        let note = poster_note(&req.output);
        req.covers =
            crate::cover::plan(&req.probe, &job.cover, job.settings.container, false, temp).embed;
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
        text + &note
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
    /// Each row's worst issue, so progress updates don't rerun every check.
    worst: RefCell<HashMap<u64, Option<(checks::Severity, String)>>>,
    pub editor: RefCell<Option<Rc<Editor>>>,
    pgs_queue: RefCell<VecDeque<(u64, PgsQuestion)>>,
    dialog_open: RefCell<bool>,
    /// Language groups whose subtitle list is expanded in the editor.
    pub subs_expanded: RefCell<HashSet<String>>,
    quit_pending: RefCell<bool>,
    helpers: Helpers,
    tx: async_channel::Sender<Msg>,
    /// A debounced preferences save is scheduled.
    save_pending: std::cell::Cell<bool>,
    /// The alass download is running (its button is greyed out meanwhile).
    pub alass_installing: std::cell::Cell<bool>,
}

thread_local! {
    /// The one window of this process (the application is single-instance).
    static INSTANCE: std::cell::RefCell<Option<Rc<App>>> = const { std::cell::RefCell::new(None) };
}

/// Show the window, creating it on first use, and queue `files`. A second
/// launch of the app lands here too (via D-Bus), so two processes never keep
/// separate copies of the preferences and overwrite each other's presets.
pub fn build(app: &adw::Application, files: Vec<PathBuf>) {
    if let Some(a) = INSTANCE.with(|i| i.borrow().clone()) {
        a.window.present();
        if !files.is_empty() {
            a.add_files(files);
        }
        return;
    }
    let (tx, rx) = async_channel::unbounded::<Msg>();
    let (prefs, prefs_problem) = Prefs::load_reporting();

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
    section.append(Some("Add _Subtitles to Videos…"), Some("win.add-subtitles"));
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
            replace_pending: None,
            est_gen: HashMap::new(),
            est_cancel: HashMap::new(),
            est_cache: HashMap::new(),
            vmaf_cancel: None,
            crop_cancel: HashMap::new(),
            post_vmaf: HashMap::new(),
            pgs_session: HashMap::new(),
            pgs_batch: HashMap::new(),
        }),
        rows: RefCell::new(HashMap::new()),
        worst: RefCell::new(HashMap::new()),
        editor: RefCell::new(None),
        pgs_queue: RefCell::new(VecDeque::new()),
        dialog_open: RefCell::new(false),
        subs_expanded: RefCell::new(HashSet::new()),
        quit_pending: RefCell::new(false),
        helpers: Helpers::default(),
        tx,
        save_pending: std::cell::Cell::new(false),
        alass_installing: std::cell::Cell::new(false),
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
    INSTANCE.with(|i| *i.borrow_mut() = Some(app_rc.clone()));
    if let Some(msg) = prefs_problem {
        let t = adw::Toast::new(&msg);
        t.set_timeout(0);
        app_rc.toasts.add_toast(t);
    }
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
    add(
        "add-subtitles",
        Box::new(move || a.pick_videos_for_subtitles()),
    );
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

    /// Save the preferences 500 ms after the last change, so dragging a slider
    /// or typing doesn't write config.json on every step.
    pub fn save_prefs(&self) {
        if self.save_pending.replace(true) {
            return;
        }
        glib::timeout_add_local_once(Duration::from_millis(500), || {
            if let Some(a) = INSTANCE.with(|i| i.borrow().clone()) {
                a.flush_prefs();
            }
        });
    }

    /// Write a pending preferences save now (on quit).
    pub fn flush_prefs(&self) {
        if self.save_pending.replace(false) {
            self.state.borrow().prefs.save();
        }
    }

    fn pick_files(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Videos"));
        filter.add_mime_type("video/*");
        for ext in subs::VIDEO_EXTS {
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
        self.add_files_as(paths, JobMode::Encode);
    }

    /// Queue videos as jobs of `mode`. Subtitle files among `paths` are given
    /// to the video they belong to (`Movie.no.srt` → `Movie.mkv`).
    pub fn add_files_as(self: &Rc<Self>, paths: Vec<PathBuf>, mode: JobMode) {
        self.expand_zips(paths, AfterUnzip::Add(mode));
    }

    /// `add_files_as` once any zips are unpacked.
    fn add_unpacked(self: &Rc<Self>, paths: Vec<PathBuf>, mode: JobMode) {
        if paths.is_empty() {
            return;
        }
        let (srts, paths): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|p| subs::is_srt(p));
        let (images, paths): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|p| crate::cover::is_image(p));
        let mut new_ids = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            let batch = st.next_batch;
            st.next_batch += 1;
            let preset = st.prefs.last_preset.clone();
            let settings = st.prefs.last_settings.clone();
            let (save_poster, poster_crop) = (st.prefs.save_poster, st.prefs.poster_crop);
            for p in paths {
                if st.jobs.iter().any(|j| {
                    j.input == p && !matches!(j.state, JobState::Done | JobState::Cancelled)
                }) {
                    continue;
                }
                let id = st.next_id;
                st.next_id += 1;
                let mut job = Job::new(id, batch, p, settings.clone(), preset.clone());
                // Sidecars next to the video arrive with the probe result.
                job.mode = mode;
                job.cover.save_poster = save_poster;
                job.cover.poster_crop = poster_crop;
                st.jobs.push(job);
                new_ids.push(id);
            }
        }
        if !srts.is_empty() {
            self.attach_subtitles(srts, &new_ids, None);
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
        // A picture dropped with videos becomes their cover art; dropped on
        // its own, the selected job's.
        if let Some(img) = images.into_iter().next() {
            let targets = if new_ids.is_empty() {
                self.state.borrow().selected.into_iter().collect()
            } else {
                new_ids.clone()
            };
            if targets.is_empty() {
                self.toast("Select a job first, then drop the picture to use it as cover art");
            }
            for id in targets {
                self.set_cover_image(id, img.clone());
            }
        }
        if self.state.borrow().jobs.is_empty() {
            return;
        }
        self.stack.set_visible_child_name("jobs");
        if let Some(first) = new_ids.first()
            && self.state.borrow().selected.is_none()
        {
            self.select_row(*first);
        }
        self.update_start();
    }

    /// Replace each `.zip` among `paths` with the SRT files it holds, then
    /// carry on with `then`. Zips are unpacked on a worker thread (a zip on a
    /// sleeping NAS mustn't freeze the window); without any, `then` runs now.
    fn expand_zips(self: &Rc<Self>, paths: Vec<PathBuf>, then: AfterUnzip) {
        if !paths.iter().any(|p| subs::is_zip(p)) {
            self.after_unzip(paths, Vec::new(), then);
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (paths, errors) = unzip_all(paths);
            let _ = tx.send_blocking(Msg::Unzipped(paths, errors, then));
        });
    }

    fn after_unzip(self: &Rc<Self>, paths: Vec<PathBuf>, errors: Vec<String>, then: AfterUnzip) {
        if !errors.is_empty() {
            self.toast(&format!("{}.", errors.join("; ")));
        }
        match then {
            AfterUnzip::Add(mode) => self.add_unpacked(paths, mode),
            AfterUnzip::Attach(id) => {
                if !paths.is_empty() {
                    self.attach_subtitles(paths, &[], Some(id));
                }
            }
        }
    }

    /// Give each subtitle file to the queued video it belongs to; files that
    /// match nothing go to the only new video, or else to the selected job.
    pub fn attach_subtitles(
        self: &Rc<Self>,
        srts: Vec<PathBuf>,
        new_ids: &[u64],
        target: Option<u64>,
    ) {
        let mut touched: Vec<u64> = Vec::new();
        let mut unmatched: Vec<String> = Vec::new();
        let mut busy: Vec<String> = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            let sync = st.prefs.sync_subs;
            let selected = st.selected;
            let fallback = match new_ids {
                [one] => Some(*one),
                [] => selected.filter(|id| st.job(*id).is_some_and(|j| j.state.editable())),
                _ => None,
            };
            for srt in srts {
                let open = |j: &Job| j.state.editable() || j.state == JobState::Probing;
                // The video it belongs to, among every job: the longest name
                // wins, and an open job wins a tie with a finished one.
                let named = st
                    .jobs
                    .iter()
                    .filter(|j| subs::belongs_to(&j.input, &srt))
                    .max_by_key(|j| (j.input.file_stem().map_or(0, |s| s.len()), open(j)));
                let owner = match (target, named) {
                    (Some(t), _) => Some(t),
                    (None, Some(j)) if open(j) => Some(j.id),
                    (None, Some(j)) => {
                        busy.push(format!(
                            "{} belongs to {}, which has already started",
                            srt.file_name().unwrap_or_default().to_string_lossy(),
                            j.file_name()
                        ));
                        continue;
                    }
                    (None, None) => fallback,
                };
                let Some(id) = owner else {
                    unmatched.push(
                        srt.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    );
                    continue;
                };
                let Some(j) = st.job_mut(id) else { continue };
                if !j.externals.iter().any(|e| e.path == srt) {
                    let e = ExternalSub::from_path(&j.input, srt, sync);
                    j.externals.push(e);
                }
                j.externals_changed();
                if !touched.contains(&id) {
                    touched.push(id);
                }
            }
        }
        if !unmatched.is_empty() {
            self.toast(&format!(
                "No video to add {} to. Add the video too, or select its job first.",
                unmatched.join(", ")
            ));
        }
        if !busy.is_empty() {
            self.toast(&format!("Not added: {}.", busy.join("; ")));
        }
        for id in touched {
            if !new_ids.contains(&id) {
                self.changed(id);
                if self.state.borrow().selected == Some(id) {
                    self.rebuild_editor();
                }
            }
        }
        self.update_start();
        self.run_next();
    }

    /// File dialog for SRT files (or zips of them) to add to job `id`.
    pub fn pick_subtitles(self: &Rc<Self>, id: u64) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("SubRip subtitles (.srt, .zip)"));
        filter.add_suffix("srt");
        filter.add_suffix("zip");
        filter.add_mime_type("application/x-subrip");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let folder = self
            .state
            .borrow()
            .job(id)
            .and_then(|j| j.input.parent().map(gio::File::for_path));
        let dialog = gtk::FileDialog::builder()
            .title("Add Subtitle Files")
            .modal(true)
            .filters(&filters)
            .build();
        if let Some(f) = folder {
            dialog.set_initial_folder(Some(&f));
        }
        let a = self.clone();
        dialog.open_multiple(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            if let Ok(model) = res {
                let paths: Vec<PathBuf> = (0..model.n_items())
                    .filter_map(|i| model.item(i).and_downcast::<gio::File>()?.path())
                    .collect();
                a.expand_zips(paths, AfterUnzip::Attach(id));
            }
        });
    }

    /// Pick a picture to use as job `id`'s cover art.
    pub fn pick_cover(self: &Rc<Self>, id: u64) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        for ext in crate::cover::IMAGE_SUFFIXES {
            filter.add_suffix(ext);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let folder = self
            .state
            .borrow()
            .job(id)
            .and_then(|j| j.input.parent().map(gio::File::for_path));
        let dialog = gtk::FileDialog::builder()
            .title("Choose Cover Art")
            .modal(true)
            .filters(&filters)
            .build();
        if let Some(f) = folder {
            dialog.set_initial_folder(Some(&f));
        }
        let a = self.clone();
        dialog.open(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            if let Some(path) = res.ok().and_then(|f| f.path()) {
                a.set_cover_image(id, path);
            }
        });
    }

    fn set_cover_image(self: &Rc<Self>, id: u64, path: PathBuf) {
        let editable = self
            .state
            .borrow()
            .job(id)
            .is_some_and(|j| j.state.editable() || j.state == JobState::Probing);
        if !editable {
            self.toast("That job can't be changed any more");
            return;
        }
        self.edit_cover(id, |c| c.action = crate::cover::CoverAction::Replace(path));
    }

    /// Change job `id`'s cover art choice, then rebuild the editor. Unlike
    /// `edit`, this keeps the quality score: the picture doesn't change it.
    /// The poster switches are remembered for new jobs.
    pub fn edit_cover(self: &Rc<Self>, id: u64, f: impl FnOnce(&mut crate::cover::CoverChoice)) {
        {
            let mut st = self.state.borrow_mut();
            let Some(j) = st
                .job_mut(id)
                .filter(|j| j.state.editable() || j.state == JobState::Probing)
            else {
                return;
            };
            f(&mut j.cover);
            let (save, crop) = (j.cover.save_poster, j.cover.poster_crop);
            st.prefs.save_poster = save;
            st.prefs.poster_crop = crop;
        }
        self.save_prefs();
        self.changed(id);
        self.rebuild_editor();
    }

    /// File dialog for videos that get the "subtitles only" treatment.
    fn pick_videos_for_subtitles(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Videos and subtitles"));
        filter.add_mime_type("video/*");
        for ext in ["mkv", "mp4", "m4v", "mov", "srt", "zip"] {
            filter.add_suffix(ext);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Add Subtitles to Videos")
            .modal(true)
            .filters(&filters)
            .build();
        let a = self.clone();
        dialog.open_multiple(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            if let Ok(model) = res {
                let paths: Vec<PathBuf> = (0..model.n_items())
                    .filter_map(|i| model.item(i).and_downcast::<gio::File>()?.path())
                    .collect();
                let target = a.state.borrow().prefs.subs_target;
                a.add_files_as(paths, JobMode::Fix(target));
            }
        });
    }

    /// Switch a job between encoding and subtitles only.
    pub fn set_mode(self: &Rc<Self>, id: u64, mode: JobMode) {
        // Coming from an encode with audio delays set: next to the video
        // can't apply them, so start with a new file.
        let requested = mode;
        let mode = match (mode, self.state.borrow().job(id)) {
            (JobMode::Fix(SubsTarget::Sidecar), Some(j))
                if j.mode == JobMode::Encode && !subs::audio_delays(&j.selection).is_empty() =>
            {
                JobMode::Fix(SubsTarget::NewFile)
            }
            (m, _) => m,
        };
        let needs_analysis = {
            let mut st = self.state.borrow_mut();
            // Remember only a result the user picked, not the switch above.
            if let JobMode::Fix(t) = mode
                && mode == requested
            {
                st.prefs.subs_target = t;
            }
            st.job(id).is_some_and(|j| {
                mode == JobMode::Encode && j.probe.is_some() && j.crop == CropState::Unknown
            })
        };
        self.edit(id, |j| {
            j.mode = mode;
            j.subs_ok.clear();
        });
        if needs_analysis {
            self.detect_crop(id);
        }
        self.reopen_pgs(id);
        self.rebuild_editor();
        self.update_start();
    }

    /// Queue the "only PGS" questions a job hasn't had answered: ones held back
    /// while it was a subtitles-only job or while an external SRT covered the
    /// language. Languages already decided for its batch are left as they are.
    pub fn reopen_pgs(self: &Rc<Self>, id: u64) {
        let new: Vec<PgsQuestion> = {
            let mut st = self.state.borrow_mut();
            let prefs = st.prefs.preferred_languages.clone();
            let Some(j) = st.job(id) else { return };
            if j.mode != JobMode::Encode || j.state != JobState::Pending {
                return;
            }
            let Some(p) = &j.probe else { return };
            let (_, _, qs) = tracks::auto_select(p, &prefs, &j.settings);
            let new: Vec<PgsQuestion> = qs
                .into_iter()
                .filter(|q| {
                    !subs::covers_language(&j.externals, &q.family)
                        && !j.pgs_pending.iter().any(|x| x.family == q.family)
                        && !st.pgs_batch.contains_key(&(j.batch, q.family.clone()))
                        && !st.pgs_session.contains_key(&q.family)
                })
                .collect();
            if let Some(j) = st.job_mut(id) {
                j.pgs_pending.extend(new.iter().cloned());
            }
            new
        };
        if new.is_empty() {
            return;
        }
        for q in new {
            self.pgs_queue.borrow_mut().push_back((id, q));
        }
        self.update_row(id);
        self.process_pgs();
    }

    fn probe(&self, id: u64) {
        let (ffprobe, path) = {
            let st = self.state.borrow();
            let Some(j) = st.job(id) else { return };
            (st.tools.ffprobe.clone(), j.input.clone())
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _slot = runner::PROBES.acquire(None);
            let r = probe::run(&ffprobe, &path).map(Box::new);
            // Reading the folder can be slow on a NAS: keep it off the UI thread.
            let sidecars = subs::discover(&path)
                .into_iter()
                .map(|s| {
                    let mut e = ExternalSub::from_path(&path, s, false);
                    e.auto = true;
                    e
                })
                .collect();
            let _ = tx.send_blocking(Msg::Probed(id, r, sidecars));
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
        let Some(job) = st.job(id) else {
            self.worst.borrow_mut().remove(&id);
            return;
        };
        let worst = st
            .issues(job)
            .into_iter()
            .next()
            .map(|i| (i.severity, i.message));
        self.worst.borrow_mut().insert(id, worst.clone());
        if let Some(r) = self.rows.borrow().get(&id) {
            r.update(job, worst);
        }
    }

    /// Progress and stage updates: settings haven't changed, so reuse the
    /// last computed issue instead of rerunning every check.
    fn update_row_progress(&self, id: u64) {
        let cached = self.worst.borrow().get(&id).cloned();
        let Some(worst) = cached else {
            return self.update_row(id);
        };
        let st = self.state.borrow();
        if let (Some(job), Some(r)) = (st.job(id), self.rows.borrow().get(&id)) {
            r.update(job, worst);
        }
    }

    fn update_start(&self) {
        let st = self.state.borrow();
        let ready = queue::next_pending(&st.jobs).is_some();
        let running = st.running.is_some();
        self.start_btn
            .set_sensitive(ready && !running && st.replace_pending.is_none());
        self.start_btn
            .set_label(if running { "Encoding…" } else { "Start" });
    }

    /// Called after any edit to a job's settings or tracks.
    pub fn changed(self: &Rc<Self>, id: u64) {
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            if let Some(j) = st.jobs.iter().find(|j| j.id == id) {
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
            Msg::AlassInstalled(r) => self.on_alass_installed(r),
            Msg::Unzipped(paths, errors, then) => self.after_unzip(paths, errors, then),
            Msg::Probed(id, r, sidecars) => self.on_probed(id, r, sidecars),
            Msg::Analyzed(id, r) => {
                let mut st = self.state.borrow_mut();
                st.crop_cancel.remove(&id);
                if let Some(j) = st.job_mut(id) {
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
                drop(st);
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
                    let current = st.job(id).map(|j| st.estimate_key(j));
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
            Msg::Vmaf(id, key, r) => {
                {
                    let mut st = self.state.borrow_mut();
                    if st.vmaf_cancel.as_ref().is_some_and(|(j, _)| *j == id) {
                        st.vmaf_cancel = None;
                    }
                    // Settings, crop or scan changed while it ran: the score is stale.
                    let current = st.job(id).is_some_and(|j| st.estimate_key(j) == key);
                    if let Some(j) = st.job_mut(id) {
                        j.vmaf = match r {
                            _ if !current => VmafState::None,
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
                self.update_row_progress(id);
                if let Some(ed) = self.editor.borrow().as_ref().filter(|e| e.job == id) {
                    ed.update_progress(&self.state.borrow());
                }
            }
            Msg::Stage(id, s) => {
                if let Some(j) = self.state.borrow_mut().job_mut(id) {
                    j.stage = Some(s);
                }
                self.update_row_progress(id);
            }
            Msg::Finished(id, o) => self.on_finished(id, o),
            Msg::PostVmaf(id, line) => {
                {
                    let mut st = self.state.borrow_mut();
                    st.post_vmaf.remove(&id);
                    if let Some(j) = st.job_mut(id) {
                        match j.report.iter_mut().find(|l| *l == POST_VMAF_PENDING) {
                            Some(l) => *l = line,
                            None => j.report.push(line),
                        }
                    }
                }
                self.update_row(id);
                if self.state.borrow().selected == Some(id) {
                    self.rebuild_editor();
                }
            }
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

    fn on_probed(
        self: &Rc<Self>,
        id: u64,
        r: Result<Box<ProbeInfo>, String>,
        sidecars: Vec<ExternalSub>,
    ) {
        let mut questions = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            let prefs = st.prefs.preferred_languages.clone();
            let sync = st.prefs.sync_subs;
            let Some(j) = st.job_mut(id) else { return };
            match r {
                Ok(p) => {
                    let (mut sel, groups, mut qs) = tracks::auto_select(&p, &prefs, &j.settings);
                    // Sidecars found next to the video. For a re-encode keep
                    // the preferred languages (and unknown ones); files the
                    // user added by hand are left as they are.
                    let fams = tracks::resolve_preferences(&p, &prefs);
                    for mut e in sidecars {
                        if j.externals.iter().any(|x| x.path == e.path) {
                            continue;
                        }
                        e.sync = sync;
                        if j.mode == JobMode::Encode
                            && let Some(l) = &e.language
                        {
                            e.selected = fams.contains(&tracks::family(l));
                        }
                        j.externals.push(e);
                    }
                    subs::prefer_externals(&mut sel, &p, &j.externals);
                    qs.retain(|q| !subs::covers_language(&j.externals, &q.family));
                    // Subtitles-only jobs keep every embedded track; nothing to ask.
                    if j.mode.fix_only() {
                        qs.clear();
                    }
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
        let encode = self
            .state
            .borrow()
            .job(id)
            .is_some_and(|j| j.mode == JobMode::Encode);
        // Crop/interlace analysis and size estimates only matter for encodes.
        if encode {
            self.detect_crop(id);
            self.schedule_estimate(id);
        }
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
            let r = match runner::HEAVY.acquire(Some(&cancel)) {
                Some(_slot) => worker::analyze(&tools, &probe, &cancel),
                None => Err("Cancelled".into()),
            };
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
        if job.mode.fix_only() {
            return;
        }
        let Some(p) = job.probe.as_ref() else { return };
        let (audio, subs) = (
            job.selection.audio_bytes(p),
            job.selection.subtitle_bytes(p) + job.external_bytes(),
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
            let key = st.estimate_key(&job);
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
                    let r = match runner::HEAVY.acquire(Some(&cancel)) {
                        Some(_slot) => worker::estimate_video(&tools, &req, &cancel),
                        None => Err("Cancelled".into()),
                    };
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
        let key = st.estimate_key(&job);
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
            let r = match runner::HEAVY.acquire(Some(&cancel)) {
                Some(_slot) => worker::vmaf_samples(&tools, &req, &cancel),
                None => Err("Cancelled".into()),
            };
            // A cancelled run's job state was already reset by `stop_vmaf`.
            if !cancel.is_cancelled() {
                let _ = tx.send_blocking(Msg::Vmaf(id, key, r));
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
        self.flush_prefs();
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            for c in st
                .est_cancel
                .values()
                .chain(st.crop_cancel.values())
                .chain(st.post_vmaf.values())
            {
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
            self.confirm_replacements();
            return;
        }
        let d = adw::AlertDialog::new(Some("Start Anyway?"), Some(&warnings.join("\n\n")));
        d.add_responses(&[("cancel", "Cancel"), ("start", "Start Anyway")]);
        d.set_response_appearance("start", adw::ResponseAppearance::Suggested);
        d.set_close_response("cancel");
        let a = self.clone();
        d.connect_response(None, move |_, resp| {
            if resp == "start" {
                a.confirm_replacements();
            }
        });
        d.present(Some(&self.window));
    }

    /// Ask once, up front, about every queued job whose output already exists,
    /// so an unattended queue doesn't stop half-way at a "Replace?" dialog.
    fn confirm_replacements(self: &Rc<Self>) {
        // Subtitles-only jobs that would replace files: embeds rewrite the video,
        // sidecars may land on an existing file that isn't their own source.
        #[allow(clippy::type_complexity)]
        let subs_jobs: Vec<(u64, String, Vec<PathBuf>)> = {
            let st = self.state.borrow();
            st.jobs
                .iter()
                .filter(|j| j.state == JobState::Pending)
                .filter_map(|j| {
                    let files = j.unapproved_replacements();
                    if files.is_empty() {
                        return None;
                    }
                    let line = if j.mode == JobMode::Fix(SubsTarget::Embed) {
                        format!("• {} (remuxed in place)", j.input.display())
                    } else {
                        files
                            .iter()
                            .map(|o| format!("• {}", o.display()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    Some((j.id, line, files))
                })
                .collect()
        };
        if !subs_jobs.is_empty() {
            let list: Vec<&str> = subs_jobs.iter().map(|(_, l, _)| l.as_str()).collect();
            let d = adw::AlertDialog::new(
                Some("Replace Files?"),
                Some(&format!(
                    "These files will be replaced. Replacing a video writes a new copy next to it, checks it, and only then replaces the original; it needs free space for one more copy.\n\n{}",
                    list.join("\n")
                )),
            );
            d.add_responses(&[
                ("cancel", "Cancel"),
                ("skip", "Skip Those Jobs"),
                ("replace", "Replace"),
            ]);
            d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
            d.set_close_response("cancel");
            let a = self.clone();
            d.connect_response(None, move |_, resp| {
                if resp == "cancel" {
                    return;
                }
                for (id, _, files) in &subs_jobs {
                    let mut st = a.state.borrow_mut();
                    let Some(j) = st.job_mut(*id).filter(|j| j.state == JobState::Pending) else {
                        continue;
                    };
                    if resp == "replace" {
                        j.subs_ok.extend(files.iter().cloned());
                    } else {
                        j.apply(Event::Cancel);
                        j.error = Some("Skipped: it would replace files".into());
                    }
                    drop(st);
                    a.update_row(*id);
                }
                a.confirm_replacements();
            });
            d.present(Some(&self.window));
            return;
        }
        let existing: Vec<(u64, PathBuf)> = {
            let st = self.state.borrow();
            st.jobs
                .iter()
                .filter(|j| j.state == JobState::Pending && j.writes_output())
                .map(|j| (j.id, st.output_for(j)))
                .filter(|(id, out)| {
                    out.exists() && st.job(*id).and_then(|j| j.replace_ok.as_ref()) != Some(out)
                })
                .collect()
        };
        let begin = |a: &Rc<Self>| {
            a.state.borrow_mut().queue_active = true;
            a.run_next();
            a.update_start();
        };
        if existing.is_empty() {
            begin(self);
            return;
        }
        let list: Vec<String> = existing
            .iter()
            .map(|(_, p)| format!("• {}", p.display()))
            .collect();
        let d = adw::AlertDialog::new(
            Some("Replace Existing Files?"),
            Some(&format!(
                "These output files already exist. Replacing overwrites their contents.\n\n{}",
                list.join("\n")
            )),
        );
        d.add_responses(&[
            ("cancel", "Cancel"),
            ("skip", "Skip Those Jobs"),
            ("replace", "Replace All"),
        ]);
        d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        d.set_close_response("cancel");
        let a = self.clone();
        d.connect_response(None, move |_, resp| {
            if resp == "cancel" {
                return;
            }
            let mut skipped = Vec::new();
            for (id, out) in &existing {
                let mut st = a.state.borrow_mut();
                let Some(j) = st.job_mut(*id).filter(|j| j.state == JobState::Pending) else {
                    continue;
                };
                if resp == "replace" {
                    j.replace_ok = Some(out.clone());
                } else {
                    j.apply(Event::Cancel);
                    j.error = Some("Output file already exists".into());
                    skipped.push(*id);
                }
            }
            for id in skipped {
                a.update_row(id);
            }
            begin(&a);
        });
        d.present(Some(&self.window));
    }

    fn run_next(self: &Rc<Self>) {
        let (id, output) = {
            let st = self.state.borrow();
            if !st.queue_active || st.running.is_some() || st.replace_pending.is_some() {
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
            if j.mode == JobMode::Encode
                && let Some(reason) = st.codec_unavailable(&j.settings)
            {
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
            (id, st.output_for(j))
        };
        // Files this job would replace that nobody has approved: an output
        // that appeared after Start was pressed, a job added while the queue
        // runs, or a subtitles job edited since it was approved.
        let (unapproved, embed) = {
            let st = self.state.borrow();
            let Some(j) = st.job(id) else { return };
            let list = if j.mode.in_place() {
                j.unapproved_replacements()
            } else if j.writes_output() && output.exists() && j.replace_ok.as_ref() != Some(&output)
            {
                vec![output.clone()]
            } else {
                Vec::new()
            };
            (list, j.mode == JobMode::Fix(SubsTarget::Embed))
        };
        if !unapproved.is_empty() {
            self.state.borrow_mut().replace_pending = Some(id);
            self.update_start();
            let body = if embed {
                format!(
                    "“{}” will be remuxed and replaced by the new copy once it checks out.",
                    output.display()
                )
            } else {
                let names: Vec<String> = unapproved
                    .iter()
                    .map(|p| format!("“{}”", p.display()))
                    .collect();
                format!(
                    "{} already exists. Replacing it will overwrite its contents.",
                    names.join(", ")
                )
            };
            let d = adw::AlertDialog::new(Some("Replace Existing File?"), Some(&body));
            d.add_responses(&[("skip", "Skip Job"), ("replace", "Replace")]);
            d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
            d.set_close_response("skip");
            let a = self.clone();
            d.connect_response(None, move |_, resp| {
                a.state.borrow_mut().replace_pending = None;
                // The job may have been removed or edited while the dialog was open.
                let pending = a
                    .state
                    .borrow()
                    .job(id)
                    .is_some_and(|j| j.state == JobState::Pending);
                if resp == "replace" && pending {
                    if let Some(j) = a.state.borrow_mut().job_mut(id) {
                        if j.mode.in_place() {
                            j.subs_ok.extend(unapproved.iter().cloned());
                        } else {
                            j.replace_ok = unapproved.first().cloned();
                        }
                    }
                    a.launch(id);
                } else {
                    if pending && let Some(j) = a.state.borrow_mut().job_mut(id) {
                        j.apply(Event::Cancel);
                        j.error = Some("Output file already exists".into());
                    }
                    a.update_row(id);
                    a.run_next();
                }
                a.update_start();
            });
            d.present(Some(&self.window));
        } else {
            self.launch(id);
        }
    }

    fn launch(self: &Rc<Self>, id: u64) {
        let (req, tools) = {
            let mut st = self.state.borrow_mut();
            // One encode at a time, and only a job that hasn't started.
            if st.running.is_some() {
                return;
            }
            let Some(job) = st.job(id).cloned() else {
                return;
            };
            if job.state != JobState::Pending {
                return;
            }
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
            // A new-file fix job that only saves a poster writes no file.
            let unused = j.mode == JobMode::Fix(SubsTarget::NewFile) && !j.writes_output();
            j.output = (!unused).then(|| req.output.clone());
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
        if state == Some(JobState::Done) {
            self.start_post_vmaf(id);
        }
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

    /// Score the finished output in the background (opt-in), so the next job
    /// can start straight away.
    fn start_post_vmaf(self: &Rc<Self>, id: u64) {
        let mut st = self.state.borrow_mut();
        let wanted = st.prefs.vmaf_after_encode && st.caps.as_ref().is_some_and(|c| c.libvmaf);
        let Some(job) = st
            .job(id)
            .filter(|j| j.settings.codec != VideoCodec::Copy && !j.mode.fix_only())
            .cloned()
        else {
            return;
        };
        if !wanted {
            return;
        }
        let Some(req) = st.request(&job) else { return };
        let cancel = Cancel::default();
        st.post_vmaf.insert(id, cancel.clone());
        let tools = st.tools.clone();
        if let Some(j) = st.job_mut(id) {
            j.report.push(POST_VMAF_PENDING.into());
        }
        drop(st);
        let tx = self.tx.clone();
        let guard = self.helpers.enter();
        std::thread::spawn(move || {
            let _guard = guard;
            let _slot = runner::HEAVY.acquire(Some(&cancel));
            let line = match worker::vmaf_output(&tools, &req, &cancel) {
                Ok(r) => crate::ffmpeg::vmaf::describe(r, req.probe.hdr.is_hdr()),
                Err(e) => format!("VMAF failed: {e}"),
            };
            if !cancel.is_cancelled() {
                let _ = tx.send_blocking(Msg::PostVmaf(id, line));
            }
        });
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
            for map in [&mut st.est_cancel, &mut st.crop_cancel, &mut st.post_vmaf] {
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
        let (src, targets) = {
            let st = self.state.borrow();
            let Some(src) = st.job(id).cloned() else {
                return;
            };
            let targets: Vec<u64> = st
                .jobs
                .iter()
                .filter(|j| j.id != id && j.state == JobState::Pending)
                .map(|j| j.id)
                .collect();
            (src, targets)
        };
        // Through `edit`, so each job gets the same follow-up as a hand edit:
        // Dolby Vision switched off when the new settings rule it out, a stale
        // VMAF score cleared, a new estimate.
        let n = targets.len();
        for t in targets {
            self.edit(t, |j| {
                let reset = j.settings.audio_defaults_differ(&src.settings);
                j.settings = src.settings.clone();
                j.preset = src.preset.clone();
                if reset && let Some(p) = &j.probe {
                    j.selection.reset_actions(p, &j.settings);
                }
            });
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
            let reset = j.settings.audio_defaults_differ(&p.settings);
            j.settings = p.settings.clone();
            j.preset = p.name.clone();
            if reset && let Some(pr) = &j.probe {
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
        if fix == checks::Fix::InstallAlass {
            self.install_alass();
            return;
        }
        if fix == checks::Fix::TracksOnly {
            // The remembered result; set_mode turns "next to the video" into
            // a new file, since that can't apply the delay.
            let t = self.state.borrow().prefs.subs_target;
            self.set_mode(id, JobMode::Fix(t));
            return;
        }
        self.edit(id, |j| {
            if fix == checks::Fix::NoCrop {
                j.crop_enabled = false;
            } else if let Some(p) = j.probe.clone() {
                checks::apply_fix(fix, &p, &mut j.settings, &mut j.selection);
            }
        });
        self.rebuild_editor();
    }

    /// Download alass in the background; the checks update when it's done.
    fn install_alass(self: &Rc<Self>) {
        if self.alass_installing.replace(true) {
            return;
        }
        self.rebuild_editor();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(Msg::AlassInstalled(crate::alass::install()));
        });
    }

    fn on_alass_installed(self: &Rc<Self>, r: Result<String, String>) {
        self.alass_installing.set(false);
        match r {
            Ok(path) => {
                if let Some(c) = self.state.borrow_mut().caps.as_mut() {
                    c.alass = Some(path);
                }
                self.toast("alass installed; subtitles will be synced automatically.");
                let ids: Vec<u64> = self.state.borrow().jobs.iter().map(|j| j.id).collect();
                for id in ids {
                    self.update_row(id);
                }
                self.rebuild_editor();
            }
            Err(e) => {
                self.toast(&format!(
                    "Couldn't install alass. {e}. The ? next to the check shows how to install it by hand."
                ));
                self.rebuild_editor();
            }
        }
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
            .release_notes(crate::changelog::to_release_notes(crate::changelog::MARKDOWN))
            .build();
        // AdwDialog's content isn't in the widget tree until presented; walk its child.
        if let Some(child) = about.child() {
            rename_whats_new(&child);
        }
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
                    let (mut sel, groups, qs) = tracks::auto_select(p, &prefs, &j.settings);
                    // Lip-sync offsets are measured by hand; keep them.
                    for c in &mut sel.audio {
                        if let Some(old) = j.selection.audio.iter().find(|o| o.index == c.index) {
                            c.offset_ms = old.offset_ms;
                        }
                    }
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

impl State {
    /// Cache key for sample estimates: file + video settings + the bitrate a
    /// target size resolves to (it depends on the kept audio) + crop + scan plan.
    pub fn estimate_key(&self, j: &Job) -> String {
        format!(
            "{}|{}|{:?}|{:?}|{:?}",
            j.input.display(),
            j.settings.video_cache_key(),
            self.video_kbps(j),
            j.active_crop(),
            j.scan_plan()
        )
    }
}

/// libadwaita labels the release-notes page "What's New" and covers only the
/// current version; ours holds the whole history, so call it "Changelog".
/// Matched by the page tag / row action target, not the (translated) text.
fn rename_whats_new(w: &gtk::Widget) {
    if let Some(row) = w.downcast_ref::<adw::ActionRow>()
        && row
            .action_target_value()
            .and_then(|v| v.get::<String>())
            .as_deref()
            == Some("whatsnew")
    {
        row.set_title("_Changelog");
    }
    if let Some(nav) = w.downcast_ref::<adw::NavigationView>()
        && let Some(page) = nav.find_page("whatsnew")
    {
        page.set_title("Changelog");
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        rename_whats_new(&c);
        child = c.next_sibling();
    }
}
