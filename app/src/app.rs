//! Application state and lifecycle. The UI thread never blocks: every long
//! operation is a worker with a cancel token, and this file only ever pushes
//! requests and drains messages.

use crate::audio::{Pcm, Waveform};
use crate::cache::SourceCache;
use crate::errors::ErrorLog;
use crate::export;
use crate::ffmpeg::{Cancel, Tools};
use crate::import::{self, FrameRequest, FrameServer};
use crate::jobs::{self, Msg, Rx, Tx};
use crate::playback::{MixState, Player, VideoStream};
use crate::project_file;
use crate::recovery;
use crate::undo::UndoStack;
use clip_core::command::{self, HwEncoder, PCM_SAMPLE_RATE};
use clip_core::{FrameIdx, Project, Rational, Timeline, TrackId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// How far playback may run past a video stream's start before the stream is
/// restarted rather than fast-forwarded through.
///
/// The stream decodes forward from where it began, so seeking within it is
/// free but arbitrarily far ahead of it is not. This is the point where
/// starting a new decode at the target beats reading everything in between —
/// a duration, not a frame count, since it is decode work being traded off and
/// that is measured in wall-clock seconds at any rate.
const STREAM_RESTART_AFTER_SECS: i64 = 20;

/// How long after an edit from a control a further edit from that same control
/// still counts as the same gesture.
///
/// Long enough to cover the gap between frames of a slow drag, short enough
/// that coming back to a control after looking at the result is a separate
/// thing to undo. A pause longer than this mid-drag splits the drag into two
/// undo entries, which is the least wrong of the available failures.
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(1000);

/// A control whose edits arrive one per frame while it is being dragged.
///
/// Identity, not position on screen: two edits coalesce only if they came from
/// the same control, so moving from one slider to the next always starts a new
/// undo entry even mid-gesture. Discrete controls — buttons, checkboxes, combo
/// boxes — do not appear here, because each click of one is its own edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    CropRect,
    OriginalGain,
    TrackGain(usize),
    TrackOffset(usize),
    Crf,
}

/// What the first-run screen is doing.
pub enum Boot {
    /// ffmpeg is available; the app is usable.
    Ready,
    /// Nothing found — offer download or a system copy.
    Missing,
    Downloading {
        done: u64,
        total: u64,
        cancel: Cancel,
    },
    Failed(String),
}

pub struct App {
    pub tx: Tx,
    pub rx: Rx,
    pub tools: Option<Tools>,
    pub hw_encoders: Vec<HwEncoder>,
    pub boot: Boot,

    pub project: Option<Project>,
    pub cache: Option<SourceCache>,
    pub proxy: Option<PathBuf>,
    pub proxy_height: u32,
    pub import_job: Option<jobs::Job>,
    pub import_progress: f32,

    pub waveform: Option<Arc<Waveform>>,
    pub pcm: Option<Arc<Pcm>>,
    /// Decoded audio for added tracks, keyed by identity so a removal cannot
    /// misroute an in-flight decode. An entry with no matching track is inert
    /// rather than wrong.
    pub track_pcm: HashMap<TrackId, Arc<Pcm>>,
    /// Tracks with a decode in flight. A track with no PCM is not necessarily
    /// one that needs loading — it may be one that is loading right now, and
    /// starting a second decode would put two writers on one scratch file.
    pub loading_tracks: HashSet<TrackId>,
    /// Cancels every added-track decode. One token for the app rather than one
    /// per load: what it is for is the exit path, where each of these is an
    /// ffmpeg child that has to be killed rather than left behind.
    pub track_cancel: Cancel,
    pub thumbs: Vec<PathBuf>,
    pub thumb_textures: Vec<egui::TextureHandle>,

    pub frames: Option<FrameServer>,
    pub texture: Option<egui::TextureHandle>,
    pub texture_size: (u32, u32),
    pub shown_frame: Option<FrameIdx>,
    pub pending_frame: Option<FrameIdx>,
    pub generation: u64,

    /// Playhead on the SOURCE timeline, in project-fps frames.
    pub playhead: FrameIdx,
    pub selection: Option<(FrameIdx, FrameIdx)>,
    pub crop_tool: bool,
    pub crop_drag: Option<CropDrag>,

    pub player: Player,
    pub video_stream: Option<VideoStream>,
    pub shuttle: i32,

    pub export_job: Option<jobs::Job>,
    pub export_progress: f32,
    pub export_label: String,
    pub export_result: Option<Result<PathBuf, String>>,
    pub show_export: bool,
    pub output_path: Option<PathBuf>,
    /// Raised when an export is about to overwrite something or run out of
    /// room. `Some` means the user has been asked and has not answered yet.
    pub export_confirm: Option<export::Preflight>,

    pub undo: UndoStack,
    pub status: String,
    pub errors: ErrorLog,
    pub hdr_warning: bool,

    /// Where `Ctrl+S` writes without asking. `None` until the edit has been
    /// saved once, or opened from a file.
    pub save_path: Option<PathBuf>,
    /// Edits since the last save. Drives the title bar and, with it, whether
    /// there is anything worth recovering after a crash.
    pub unsaved: bool,
    /// Unsaved work from the previous run — closed without saving, or not
    /// closed at all — waiting for the user to take it or leave it.
    pub pending_recovery: Option<recovery::Pending>,
    /// Which control the last edit came from, and when — together they decide
    /// whether the next edit continues a gesture or starts a new undo entry.
    last_control: Option<Control>,
    last_edit_at: Instant,
    /// When the recovery snapshot was last written.
    last_autosave: Instant,
    /// The title currently on the window, so it is only pushed when it moves.
    shown_title: String,
}

pub struct CropDrag {
    pub handle: clip_core::crop::Handle,
    /// Pointer offset from the rect origin, for move drags.
    pub grab: egui::Vec2,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> App {
        App::with_context(&cc.egui_ctx)
    }

    /// Construction without eframe, so the UI can be driven headlessly in
    /// tests as well as by a real window.
    pub fn with_context(ctx: &egui::Context) -> App {
        let (tx, rx) = jobs::channel();
        crate::cache::cleanup();
        configure_style(ctx);

        let tools = crate::bootstrap::locate(None);
        let boot = if tools.is_some() {
            Boot::Ready
        } else {
            Boot::Missing
        };
        let hw_encoders = tools
            .as_ref()
            .map(|t| t.available_hw_encoders())
            .unwrap_or_default();
        let frames = tools
            .as_ref()
            .map(|t| FrameServer::spawn(t.clone(), tx.clone()));

        App {
            tx,
            rx,
            tools,
            hw_encoders,
            boot,
            project: None,
            cache: None,
            proxy: None,
            proxy_height: command::PROXY_HEIGHT,
            import_job: None,
            import_progress: 0.0,
            waveform: None,
            pcm: None,
            track_pcm: HashMap::new(),
            loading_tracks: HashSet::new(),
            track_cancel: Cancel::new(),
            thumbs: Vec::new(),
            thumb_textures: Vec::new(),
            frames,
            texture: None,
            texture_size: (0, 0),
            shown_frame: None,
            pending_frame: None,
            generation: 0,
            playhead: FrameIdx(0),
            selection: None,
            crop_tool: false,
            crop_drag: None,
            player: Player::new(),
            video_stream: None,
            shuttle: 0,
            export_job: None,
            export_progress: 0.0,
            export_label: String::new(),
            export_result: None,
            show_export: false,
            output_path: None,
            export_confirm: None,
            undo: UndoStack::default(),
            status: String::new(),
            errors: ErrorLog::default(),
            hdr_warning: false,
            save_path: None,
            unsaved: false,
            pending_recovery: recovery::pending(),
            last_control: None,
            last_edit_at: Instant::now(),
            last_autosave: Instant::now(),
            shown_title: String::new(),
        }
    }

    pub fn fps(&self) -> Rational {
        self.project
            .as_ref()
            .map(|p| p.project_fps)
            .unwrap_or(Rational::from_int(30))
    }

    pub fn timeline(&self) -> Timeline {
        Timeline::new(
            &self
                .project
                .as_ref()
                .map(|p| p.normalised_segments())
                .unwrap_or_default(),
            self.fps(),
        )
    }

    pub fn source_frames(&self) -> u64 {
        self.project
            .as_ref()
            .map(|p| p.source_frames().0)
            .unwrap_or(0)
    }

    // -- opening -------------------------------------------------------

    /// One Open for both kinds of file. Which one it is follows from the
    /// extension, so the user picks "the thing they want to work on" rather
    /// than first having to tell the app which menu item they meant.
    pub fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Clip project or video",
                &[
                    project_file::EXTENSION,
                    "mkv",
                    "mp4",
                    "mov",
                    "webm",
                    "avi",
                    "flv",
                    "m4v",
                    "ts",
                ],
            )
            .add_filter("Clip project", &[project_file::EXTENSION])
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    pub fn open_path(&mut self, path: PathBuf) {
        if project_file::is_project_file(&path) {
            self.open_project_path(path);
        } else {
            self.import_source(path);
        }
    }

    /// Open a video: a brand new edit over it.
    fn import_source(&mut self, path: PathBuf) {
        let Some(tools) = self.tools.clone() else {
            self.errors.push("ffmpeg is not set up yet");
            return;
        };
        match import::probe_source(&tools, &path) {
            Ok(source) => {
                if source.is_hdr {
                    // Producing washed-out grey output is not acceptable; the
                    // import path tone-maps, and the user is told so.
                    self.hdr_warning = true;
                }
                let project = Project::new(source);
                self.reset_for(project, project_file::ViewState::default());
                self.save_path = None;
                self.unsaved = false;
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Open a `.clip` file: the edit is restored, and the media it refers to
    /// is imported again behind it.
    pub fn open_project_path(&mut self, path: PathBuf) {
        let file = match project_file::load(&path) {
            Ok(f) => f,
            Err(e) => {
                self.errors.push(format!("{e:#}"));
                return;
            }
        };
        if !file.project.source.path.exists() {
            // Worth opening anyway: the edit is intact, and the user may be
            // able to point at the file or reconnect the drive it is on.
            self.errors.push(format!(
                "source file is missing: {}",
                file.project.source.path.display()
            ));
        }
        self.hdr_warning = file.project.source.is_hdr;
        self.reset_for(file.project, file.view);
        self.save_path = Some(path);
        self.unsaved = false;
        // The edit refers to audio by path, so every added track decodes again
        // — from its scratch file if this machine still has one, otherwise
        // from the file itself.
        self.reload_missing_track_audio();
    }

    /// Replace the open project and drop everything derived from the old one.
    fn reset_for(&mut self, project: Project, view: project_file::ViewState) {
        self.cancel_import();
        self.stop_playback();
        self.playhead = view.playhead;
        self.selection = view.selection;
        self.waveform = None;
        self.pcm = None;
        self.track_pcm.clear();
        self.thumbs.clear();
        self.thumb_textures.clear();
        self.texture = None;
        self.shown_frame = None;
        self.proxy = None;
        self.export_result = None;
        self.undo = UndoStack::default();
        self.output_path = Some(export::suggested_output(&project));
        self.project = Some(project);
        self.start_import();
        let playhead = self.playhead;
        self.request_frame(playhead);
    }

    // -- saving --------------------------------------------------------

    fn view_state(&self) -> project_file::ViewState {
        project_file::ViewState {
            playhead: self.playhead,
            selection: self.selection,
        }
    }

    /// `Ctrl+S`. Writes straight to the known path; asks for one the first time.
    pub fn save_project(&mut self) {
        match self.save_path.clone() {
            Some(path) => self.save_project_to(path),
            None => self.save_project_as(),
        }
    }

    /// `Ctrl+Shift+S`. Always asks, and the answer becomes the new save path.
    pub fn save_project_as(&mut self) {
        if self.project.is_none() {
            return;
        }
        let suggested = self.suggested_project_name();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Clip project", &[project_file::EXTENSION])
            .set_file_name(suggested)
            .save_file()
        else {
            return;
        };
        self.save_project_to(project_file::with_extension(path));
    }

    pub fn save_project_to(&mut self, path: PathBuf) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        match project_file::save(&path, project, &self.view_state()) {
            Ok(()) => {
                self.status = format!("Saved {}", file_label(&path));
                self.save_path = Some(path);
                self.unsaved = false;
                // The saved file is now the copy worth having.
                self.discard_recovery();
            }
            Err(e) => self.errors.push(format!("{e:#}")),
        }
    }

    /// The source file's name with a `.clip` extension — the edit is "of" that
    /// video, and that is the name the user is thinking in.
    fn suggested_project_name(&self) -> String {
        self.project
            .as_ref()
            .and_then(|p| {
                p.source
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .map(|stem| format!("{stem}.{}", project_file::EXTENSION))
            .unwrap_or_else(|| format!("untitled.{}", project_file::EXTENSION))
    }

    /// Push the title to the window when it changes — which edit is open and
    /// whether it has unsaved changes both belong there.
    pub fn sync_window_title(&mut self, ctx: &egui::Context) {
        let title = self.window_title();
        if title != self.shown_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }
    }

    /// Title bar: which edit this is, and whether it has been saved.
    pub fn window_title(&self) -> String {
        let base = crate::version::window_title();
        let Some(project) = &self.project else {
            return base;
        };
        let name = match &self.save_path {
            Some(p) => file_label(p),
            None => file_label(&project.source.path),
        };
        let mark = if self.unsaved { "• " } else { "" };
        format!("{mark}{name} — {base}")
    }

    // -- crash recovery ------------------------------------------------

    /// Write the recovery snapshot if enough time has passed. Called every
    /// frame; does nothing unless there is unsaved work to lose.
    pub fn autosave_tick(&mut self) {
        if !self.unsaved || self.project.is_none() {
            return;
        }
        if self.last_autosave.elapsed() < recovery::INTERVAL {
            return;
        }
        self.last_autosave = Instant::now();
        let Some(project) = self.project.as_ref() else {
            return;
        };
        // A failed autosave is not worth an error box: the user did not ask
        // for it, and the edit in front of them is unaffected.
        let _ = recovery::write(project, &self.view_state());
    }

    /// Take the unsaved work from the previous session.
    pub fn restore_recovery(&mut self) {
        let Some(pending) = self.pending_recovery.take() else {
            return;
        };
        self.hdr_warning = pending.file.project.source.is_hdr;
        self.reset_for(pending.file.project, pending.file.view);
        // Deliberately no save path: this was never saved anywhere, and
        // inheriting one would let Ctrl+S overwrite a file the user last saw
        // in a different state. It stays unsaved, and the snapshot stays on
        // disk until it is either saved or discarded.
        self.save_path = None;
        self.unsaved = true;
        self.reload_missing_track_audio();
    }

    /// Throw the previous session's unsaved work away.
    pub fn discard_recovery(&mut self) {
        self.pending_recovery = None;
        recovery::discard();
    }

    /// Shutting down. Unsaved work is written out one last time rather than
    /// discarded: closing the window without saving loses an edit just as
    /// thoroughly as crashing does, and the next launch can offer either one
    /// back. Everything saved leaves nothing behind.
    pub fn finish_session(&mut self) {
        match (self.unsaved, self.project.as_ref()) {
            (true, Some(project)) => {
                let _ = recovery::write(project, &self.view_state());
            }
            _ => recovery::discard(),
        }
    }

    // -- added track audio ---------------------------------------------

    /// Decode any added track that has no PCM in memory.
    ///
    /// Used after opening a project and after an undo restores a removed
    /// track. [`import::load_track`] reads the scratch file when one is there,
    /// so this is usually a file read rather than another trip through ffmpeg.
    pub fn reload_missing_track_audio(&mut self) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let missing: Vec<(TrackId, PathBuf)> = project
            .added_tracks
            .iter()
            .filter(|t| !self.track_pcm.contains_key(&t.id) && !self.loading_tracks.contains(&t.id))
            .map(|t| (t.id, t.path.clone()))
            .collect();
        for (id, path) in missing {
            self.spawn_track_load(id, path);
        }
    }

    fn spawn_track_load(&mut self, id: TrackId, path: PathBuf) {
        let (Some(tools), Some(cache)) = (self.tools.clone(), self.cache.clone()) else {
            return;
        };
        self.loading_tracks.insert(id);
        let dest = scratch_path(&cache, id);
        let tx = self.tx.clone();
        let cancel = self.track_cancel.clone();
        std::thread::Builder::new()
            .name("track-load".into())
            .spawn(move || import::load_track(tools, id, path, dest, tx, cancel))
            .expect("spawning track loader");
    }

    fn start_import(&mut self) {
        let (Some(tools), Some(project)) = (self.tools.clone(), self.project.clone()) else {
            return;
        };
        let cache = match SourceCache::open(&project.source.path, project.project_fps) {
            Ok(c) => c,
            Err(e) => {
                self.errors.push(format!("cache unavailable: {e}"));
                return;
            }
        };
        self.cache = Some(cache.clone());
        self.import_progress = 0.0;
        let job = jobs::Job::new("Importing");
        let cancel = job.cancel.clone();
        self.import_job = Some(job);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("import".into())
            .spawn(move || import::run_import(tools, project, cache, tx, cancel))
            .expect("spawning import worker");
    }

    pub fn cancel_import(&mut self) {
        if let Some(job) = self.import_job.take() {
            job.cancel.cancel();
        }
    }

    /// Changing the project fps invalidates the proxy, so the import runs
    /// again against a different cache key.
    pub fn set_project_fps(&mut self, fps: Rational) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let old = project.project_fps;
        if old == fps {
            return;
        }
        // Segments are times, so the edit itself needs no conversion: the same
        // instants simply land on a different grid. Only the cursors are
        // expressed in frames, and those convert through their exact time
        // rather than by scaling the index.
        self.edit(|p| p.project_fps = fps);
        self.playhead = regrid(self.playhead, old, fps);
        self.selection = self
            .selection
            .map(|(a, b)| (regrid(a, old, fps), regrid(b, old, fps)));
        self.cancel_import();
        self.proxy = None;
        self.waveform = None;
        self.pcm = None;
        self.thumbs.clear();
        self.thumb_textures.clear();
        self.start_import();
    }

    // -- editing -------------------------------------------------------

    /// Change the edit. **The only way to change the edit.**
    ///
    /// Everything an edit implies happens here and nowhere else: the undo
    /// entry, the unsaved marker that drives the title bar and the recovery
    /// snapshot, the playhead landing on a kept frame, the mix the audio
    /// callback reads. Before this existed each caller did its own subset, and
    /// the subsets disagreed — a crop drag or a gain change left `unsaved`
    /// false, so autosave never wrote, and closing the window discarded the
    /// snapshot on the grounds that there was nothing to lose. Nine of the
    /// fifteen edit paths were wrong in one direction or the other.
    ///
    /// The closure gets the project because the alternative — mutate first,
    /// tell the app afterwards — is the arrangement that produced the bug.
    pub fn edit(&mut self, f: impl FnOnce(&mut Project)) {
        self.apply_edit(None, f);
    }

    /// An edit from a control that produces one call per frame while it is
    /// being dragged.
    ///
    /// Consecutive edits from the same control fold into one undo entry, so a
    /// two-second drag of a gain slider is one Ctrl+Z rather than a hundred and
    /// twenty — which would also push everything else off a stack that holds a
    /// hundred. See [`COALESCE_WINDOW`] for what "consecutive" means.
    pub fn edit_continuous(&mut self, control: Control, f: impl FnOnce(&mut Project)) {
        self.apply_edit(Some(control), f);
    }

    fn apply_edit(&mut self, control: Option<Control>, f: impl FnOnce(&mut Project)) {
        let Some(before) = self.project.clone() else {
            return;
        };
        // A new entry unless this is the same control still moving. The very
        // first call of a drag cannot coalesce — whatever the user touched
        // before was something else — so the pre-drag state is always kept.
        let coalesce = control.is_some()
            && self.last_control == control
            && self.last_edit_at.elapsed() < COALESCE_WINDOW;
        if !coalesce {
            self.undo.push(before);
        }
        self.last_control = control;
        self.last_edit_at = Instant::now();

        if let Some(p) = self.project.as_mut() {
            f(p);
        }
        self.after_edit();
    }

    // -- the edit vocabulary -------------------------------------------
    //
    // One named method per thing the UI can change. The widgets do not touch
    // `Project`: they read it, draw, and ask for a change by name. That is what
    // makes the list of edits enumerable — `app/tests/undo_entry_points.rs`
    // walks it — and what keeps a new control from quietly acquiring its own
    // idea of what an edit implies.

    pub fn set_aspect_and_crop(
        &mut self,
        mode: clip_core::AspectMode,
        crop: Option<clip_core::CropRect>,
    ) {
        self.edit(|p| {
            p.aspect_mode = mode;
            p.crop = crop;
        });
    }

    /// One frame of a crop drag: continuous, so the gesture is one undo entry.
    pub fn drag_crop(&mut self, rect: clip_core::CropRect) {
        if self.project.as_ref().and_then(|p| p.crop) == Some(rect) {
            return;
        }
        self.edit_continuous(Control::CropRect, |p| p.crop = Some(rect));
    }

    pub fn set_original_audio_source(&mut self, source: clip_core::AudioSourceSelection) {
        self.edit(|p| p.original_audio.source = source);
    }

    pub fn set_original_gain(&mut self, db: f32) {
        self.edit_continuous(Control::OriginalGain, |p| p.original_audio.gain_db = db);
    }

    pub fn set_track_gain(&mut self, index: usize, db: f32) {
        self.edit_continuous(Control::TrackGain(index), |p| {
            if let Some(t) = p.added_tracks.get_mut(index) {
                t.gain_db = db;
            }
        });
    }

    /// Seconds on the output timeline. Negative is legal: the track starts
    /// before the clip does, and export handles it with an input seek.
    pub fn set_track_offset(&mut self, index: usize, seconds: f32) {
        self.edit_continuous(Control::TrackOffset(index), |p| {
            if let Some(t) = p.added_tracks.get_mut(index) {
                t.offset = clip_core::RationalTime::from_seconds_f64(seconds as f64);
            }
        });
    }

    pub fn set_track_fade_out(&mut self, index: usize, on: bool) {
        self.edit(|p| {
            if let Some(t) = p.added_tracks.get_mut(index) {
                t.fade_out = on;
            }
        });
    }

    pub fn set_output_settings(&mut self, settings: clip_core::OutputSettings) {
        self.edit(|p| p.output = settings);
    }

    pub fn set_crf(&mut self, crf: u32) {
        self.edit_continuous(Control::Crf, |p| {
            p.output.quality = clip_core::Quality::Crf(crf)
        });
    }

    /// End any run of continuous edits, so the next one starts a fresh undo
    /// entry. Anything that moves the project wholesale — undo, redo, opening
    /// a file — calls this: without it the first slider nudge after an undo
    /// would fold into the entry the undo just walked back to.
    fn end_edit_run(&mut self) {
        self.last_control = None;
    }

    pub fn undo(&mut self) {
        self.end_edit_run();
        if let Some(p) = self.project.as_mut() {
            if let Some(prev) = self.undo.undo(p.clone()) {
                *p = prev;
                self.after_edit();
                // An undone removal brings a track back; its samples were
                // dropped from memory but its scratch file was not.
                self.reload_missing_track_audio();
            }
        }
    }

    pub fn redo(&mut self) {
        self.end_edit_run();
        if let Some(p) = self.project.as_mut() {
            if let Some(next) = self.undo.redo(p.clone()) {
                *p = next;
                self.after_edit();
                self.reload_missing_track_audio();
            }
        }
    }

    /// The bookkeeping every edit needs. Private: [`edit`](Self::edit) is the
    /// door, and a caller that could reach this could also forget to.
    fn after_edit(&mut self) {
        self.unsaved = true;
        let timeline = self.timeline();
        if !timeline.is_empty() && timeline.to_output(self.playhead).is_none() {
            if let Some(out) = timeline.to_output_snapped(self.playhead) {
                if let Some(src) = timeline.to_source(out) {
                    self.seek_source(src);
                }
            }
        }
        self.refresh_mix();
    }

    pub fn refresh_mix(&mut self) {
        let Some(project) = &self.project else {
            self.player.set_state(None);
            return;
        };
        let state = MixState::build(project, self.pcm.clone(), &self.track_pcm);
        self.player.set_state(Some(Arc::new(state)));
    }

    // -- transport -----------------------------------------------------

    pub fn seek_source(&mut self, frame: FrameIdx) {
        let max = self.source_frames().saturating_sub(1);
        let frame = FrameIdx(frame.0.min(max));
        self.playhead = frame;
        let fps = self.fps();
        let timeline = self.timeline();
        if let Some(out) = timeline.to_output_snapped(frame) {
            self.player.seek_frame(out, fps);
        }
        if self.player.is_playing() {
            self.restart_video_stream();
        } else {
            self.request_frame(frame);
        }
    }

    pub fn step(&mut self, delta: i64) {
        let target = if delta < 0 {
            self.playhead.saturating_sub((-delta) as u64)
        } else {
            self.playhead.saturating_add(delta as u64)
        };
        self.pause();
        self.seek_source(target);
    }

    pub fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn play(&mut self) {
        if self.project.is_none() || self.timeline().is_empty() {
            return;
        }
        self.refresh_mix();
        self.shuttle = 1;
        self.player.play();
        self.restart_video_stream();
    }

    pub fn pause(&mut self) {
        self.shuttle = 0;
        self.player.pause();
        if let Some(s) = self.video_stream.take() {
            s.stop();
        }
        let frame = self.playhead;
        self.request_frame(frame);
    }

    pub fn stop_playback(&mut self) {
        self.pause();
        self.player.set_state(None);
    }

    fn restart_video_stream(&mut self) {
        if let Some(s) = self.video_stream.take() {
            s.stop();
        }
        let (Some(tools), Some(proxy)) = (self.tools.clone(), self.proxy.clone()) else {
            return;
        };
        let (w, h) = self.proxy_dimensions();
        if w == 0 || h == 0 {
            return;
        }
        self.video_stream = Some(VideoStream::start(
            &tools,
            proxy,
            self.playhead,
            self.fps(),
            w,
            h,
        ));
    }

    fn proxy_dimensions(&self) -> (u32, u32) {
        // The proxy is scaled to a fixed height; width follows the source
        // display aspect and must be even.
        let Some(project) = &self.project else {
            return (0, 0);
        };
        let (sw, sh) = project.source.display_size();
        if sh == 0 {
            return (0, 0);
        }
        let h = self.proxy_height;
        let w = clip_core::model::even_round(sw as f32 * h as f32 / sh as f32);
        (w, h)
    }

    // -- preview frames ------------------------------------------------

    pub fn request_frame(&mut self, frame: FrameIdx) {
        let (Some(frames), Some(project)) = (&self.frames, &self.project) else {
            return;
        };
        if self.pending_frame == Some(frame) || self.shown_frame == Some(frame) {
            return;
        }
        self.generation += 1;
        let t = frame.to_time(project.project_fps);
        let (args, height) = match &self.proxy {
            Some(proxy) => (command::proxy_frame_args(proxy, t), self.proxy_height),
            // Before the proxy exists, decode straight from the source. Slow
            // to seek, but it makes the app usable the moment a file opens.
            None => (
                command::source_frame_args(project, t, command::PROXY_HEIGHT),
                command::PROXY_HEIGHT,
            ),
        };
        self.pending_frame = Some(frame);
        frames.request(FrameRequest {
            generation: self.generation,
            frame,
            args,
            height,
        });
    }

    // -- export --------------------------------------------------------

    /// Start an export, or raise what the user should see first.
    ///
    /// Replacing a file and running out of disk twenty minutes in are both
    /// things they would want to have been told, and neither was ever said —
    /// the default output path never passes through a save dialog, so nothing
    /// asked. When [`export::preflight`] finds something, this stops and puts
    /// it on screen; the answer comes back through
    /// [`start_export_confirmed`](Self::start_export_confirmed).
    pub fn start_export(&mut self, ctx: &egui::Context) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let Some(out) = self.output_path.clone() else {
            return;
        };
        let scratch = crate::cache::cache_root().ok();
        let checks = export::preflight(project, &out, scratch.as_deref());
        if checks.is_clear() {
            self.start_export_confirmed(ctx);
        } else {
            self.export_confirm = Some(checks);
        }
    }

    /// Start the export the user has been warned about and asked for anyway.
    pub fn start_export_confirmed(&mut self, ctx: &egui::Context) {
        self.export_confirm = None;
        let (Some(tools), Some(project)) = (self.tools.clone(), self.project.clone()) else {
            return;
        };
        if project.normalised_segments().is_empty() {
            self.errors
                .push("nothing to export — the timeline is empty");
            return;
        }
        let Some(out) = self.output_path.clone() else {
            return;
        };
        let hw = self.hw_encoders.first().copied();
        let job = jobs::Job::new("Exporting");
        let cancel = job.cancel.clone();
        self.export_job = Some(job);
        self.export_progress = 0.0;
        self.export_label = "Starting…".into();
        self.export_result = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                export::run_export(tools, project, hw, out, tx, cancel);
                ctx.request_repaint();
            })
            .expect("spawning export worker");
    }

    /// Ask for the output file. Returns whether it moved — the export
    /// confirmation uses that to decide whether anything has changed since it
    /// asked, so a cancelled dialog does not put the same warning straight back
    /// on screen.
    pub fn pick_output(&mut self) -> bool {
        let Some(project) = &self.project else {
            return false;
        };
        let ext = project.output.preset.extension();
        let start = self
            .output_path
            .clone()
            .unwrap_or_else(|| export::suggested_output(project));
        let dialog = rfd::FileDialog::new()
            .add_filter(ext, &[ext])
            .set_file_name(
                start
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );
        let dialog = match start.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        match dialog.save_file() {
            Some(path) => {
                let moved = self.output_path.as_ref() != Some(&path);
                self.output_path = Some(path);
                moved
            }
            None => false,
        }
    }

    pub fn cancel_export(&mut self) {
        if let Some(job) = self.export_job.take() {
            job.cancel.cancel();
        }
    }

    // -- audio tracks --------------------------------------------------

    pub fn add_audio_track(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Audio",
                &["mp3", "wav", "m4a", "aac", "flac", "ogg", "opus"],
            )
            .pick_file()
        else {
            return;
        };
        self.add_audio_track_from_path(path);
    }

    /// The half of `add_audio_track` that does not need a file dialog, so the
    /// add/remove/decode interleaving can be driven in a test.
    pub fn add_audio_track_from_path(&mut self, path: PathBuf) {
        // Everything the load needs, checked before the snapshot so a call
        // that cannot do anything does not consume an undo step.
        if self.tools.is_none() || self.cache.is_none() || self.project.is_none() {
            return;
        }
        let track = clip_core::AudioTrack::new(path.clone());
        let id = track.id;
        self.edit(|p| p.added_tracks.push(track));
        self.spawn_track_load(id, path);
    }

    /// Remove by position — the UI acts on the list it is drawing, and that is
    /// a legitimate use of an index because it is resolved immediately.
    pub fn remove_audio_track(&mut self, index: usize) {
        // Validate before snapshotting: a removal the UI cannot satisfy must
        // not leave an undo entry that undoes nothing.
        if !self
            .project
            .as_ref()
            .is_some_and(|p| index < p.added_tracks.len())
        {
            return;
        }
        let mut removed = None;
        self.edit(|p| removed = Some(p.added_tracks.remove(index)));
        // The decoded samples go, because holding every removed track's PCM
        // for the session costs ~115 MB per five minutes of stereo. The
        // scratch file on disk stays, so an undo reads the audio back instead
        // of restoring a silent track — see `reload_missing_track_audio`. It
        // is a cache artifact and is cleaned up with the rest of the source's
        // cache entry rather than being tied to the undo stack.
        if let Some(removed) = removed {
            self.track_pcm.remove(&removed.id);
            self.refresh_mix();
        }
    }

    /// A decode finished. The track it was for may have moved, or be gone.
    fn track_ready(
        &mut self,
        id: TrackId,
        duration: Option<clip_core::RationalTime>,
        pcm: Option<Arc<Pcm>>,
    ) {
        self.loading_tracks.remove(&id);
        let track = self
            .project
            .as_mut()
            .and_then(|p| p.added_tracks.iter_mut().find(|t| t.id == id));
        let Some(track) = track else {
            // Removed while it was decoding, or removed and then undone back
            // out again. Nothing to attach the result to, so drop it — but the
            // scratch file stays, since an undo can still ask for this id.
            return;
        };
        // Only overwrite a known duration: a reload from the scratch file has
        // no probe behind it and reports `None`, which is not the same as
        // "this track has no duration".
        if duration.is_some() {
            track.duration = duration;
        }
        match pcm {
            Some(pcm) => {
                self.track_pcm.insert(id, pcm);
            }
            // A failed decode leaves the track in place but silent, rather than
            // leaving a previous track's samples under this id.
            None => {
                self.track_pcm.remove(&id);
            }
        }
        self.refresh_mix();
    }

    // -- messages ------------------------------------------------------

    pub fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status = s,
                Msg::Error(e) => self.errors.push(e),
                Msg::BootstrapProgress { done, total } => {
                    if let Boot::Downloading {
                        done: d, total: t, ..
                    } = &mut self.boot
                    {
                        *d = done;
                        *t = total;
                    }
                }
                Msg::BootstrapDone(result) => match result {
                    Ok(tools) => {
                        self.hw_encoders = tools.available_hw_encoders();
                        self.frames = Some(FrameServer::spawn(tools.clone(), self.tx.clone()));
                        self.tools = Some(tools);
                        self.boot = Boot::Ready;
                    }
                    Err(e) => self.boot = Boot::Failed(e),
                },
                Msg::ProxyProgress(f) => self.import_progress = f,
                Msg::ProxyReady(path) => {
                    self.proxy = Some(path);
                    // Re-fetch the current frame from the proxy: it is sharper
                    // than the pre-proxy direct decode and, more importantly,
                    // it is the same source every later frame comes from.
                    let frame = self.playhead;
                    self.shown_frame = None;
                    self.pending_frame = None;
                    self.request_frame(frame);
                }
                Msg::PcmReady(pcm) => {
                    self.pcm = Some(pcm);
                    self.refresh_mix();
                }
                Msg::WaveformReady(w) => self.waveform = Some(w),
                Msg::ThumbsReady(t) => {
                    self.thumbs = t;
                    self.thumb_textures.clear();
                }
                Msg::ImportFailed(e) => {
                    self.errors.push(e);
                    self.import_job = None;
                }
                Msg::TrackReady { id, duration, pcm } => self.track_ready(id, duration, pcm),
                Msg::Frame(frame) => {
                    if frame.generation >= self.generation {
                        self.upload_frame(ctx, &frame);
                        self.shown_frame = Some(frame.frame);
                        self.pending_frame = None;
                    }
                }
                Msg::FrameFailed(e) => {
                    self.pending_frame = None;
                    self.errors.push(e);
                }
                Msg::ExportProgress { fraction, label } => {
                    self.export_progress = fraction;
                    self.export_label = label;
                }
                Msg::ExportDone(result) => {
                    self.export_job = None;
                    self.export_result = Some(result);
                }
            }
        }
    }

    fn upload_frame(&mut self, ctx: &egui::Context, frame: &crate::jobs::DecodedFrame) {
        let size = [frame.width as usize, frame.height as usize];
        if size[0] * size[1] * 4 > frame.rgba.len() {
            return;
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba[..size[0] * size[1] * 4]);
        match &mut self.texture {
            Some(tex) if self.texture_size == (frame.width, frame.height) => {
                tex.set(image, egui::TextureOptions::LINEAR);
            }
            _ => {
                self.texture =
                    Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
                self.texture_size = (frame.width, frame.height);
            }
        }
    }

    /// While playing, the audio clock decides which frame is shown.
    pub fn follow_playback(&mut self, ctx: &egui::Context) {
        if !self.player.is_playing() {
            return;
        }
        let fps = self.fps();
        let timeline = self.timeline();
        let out_frame = self.player.position_frame(fps);
        let Some(src) = timeline.to_source(out_frame) else {
            // Ran off the end of the edit.
            self.pause();
            return;
        };
        // Crossing a cut means the stream is decoding the wrong part of the
        // source, so it gets restarted at the new position.
        let restart_after = FrameIdx::from_time_round(
            clip_core::RationalTime::new(STREAM_RESTART_AFTER_SECS, 1),
            fps,
        );
        let needs_restart = match &self.video_stream {
            Some(s) => src.0 < s.start_frame.0 || src.0 > s.start_frame.0 + restart_after.0,
            None => true,
        };
        self.playhead = src;
        if needs_restart {
            self.restart_video_stream();
        }
        let mut new_image = None;
        if let Some(stream) = &mut self.video_stream {
            if stream.advance_to(src) {
                if let Some((idx, bytes)) = &stream.current {
                    new_image = Some((*idx, stream.width, stream.height, bytes.clone()));
                }
            }
        }
        if let Some((idx, w, h, bytes)) = new_image {
            self.upload_frame(
                ctx,
                &crate::jobs::DecodedFrame {
                    generation: self.generation,
                    frame: idx,
                    width: w,
                    height: h,
                    rgba: bytes,
                },
            );
            self.shown_frame = Some(idx);
        }
        ctx.request_repaint();
    }

    pub fn playhead_seconds(&self) -> f64 {
        self.playhead.to_time(self.fps()).as_f64()
    }

    pub fn output_position_samples(&self) -> u64 {
        let timeline = self.timeline();
        timeline
            .to_output_snapped(self.playhead)
            .map(|f| (f.to_time(self.fps()).as_f64() * PCM_SAMPLE_RATE as f64) as u64)
            .unwrap_or(0)
    }
}

/// A path as the user thinks of it: the file name, not the whole path.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// The same instant, as a frame index on a different grid.
///
/// Cursors (playhead, selection) are frame indices because they address the
/// grid the user is looking at, so a rate change has to move them. It goes
/// through the exact time rather than scaling the index, which is what keeps
/// repeated changes from drifting — though landing on a coarser grid still
/// costs sub-frame precision, since that grid has nowhere finer to land.
fn regrid(f: FrameIdx, from: Rational, to: Rational) -> FrameIdx {
    FrameIdx::from_time_round(f.to_time(from), to)
}

/// Where a track's decoded PCM is written. Named by id so the same name is
/// never handed out to two different tracks — see [`TrackId`].
///
/// Public so a test can look at it: the file outlives the track's removal,
/// because undo reads it back.
pub fn scratch_path(cache: &SourceCache, id: TrackId) -> PathBuf {
    cache.dir.join(format!("track_{}.f32", id.0))
}

fn configure_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
    // A video tool wants a dark room, whatever the system theme says.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(24, 25, 28);
    visuals.window_fill = egui::Color32::from_rgb(30, 31, 35);
    visuals.extreme_bg_color = egui::Color32::from_rgb(16, 17, 19);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);
}
