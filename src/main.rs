#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode, TitleBar, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    empty::{Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle},
    input::{InputState, MaskPattern, NumberInput, StepAction},
    label::Label,
    notification::Notification,
    progress::Progress,
    spinner::Spinner,
    switch::Switch,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use smol::{
    channel::{Receiver, Sender},
    future,
    io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader},
    stream::StreamExt as _,
};

const MIN_SPEED: f64 = 1.1;
const MAX_SPEED: f64 = 100.;

struct VideoSpeed {
    input: Option<PathBuf>,
    /// Length of `input` in seconds, when ffprobe could read it.
    duration: Option<f64>,
    preview: Option<Arc<Image>>,
    speed: Entity<InputState>,
    remove_audio: bool,
    /// Set while ffmpeg runs; closing it cancels the job.
    job: Option<Sender<()>>,
    /// Percent done, or `None` when the duration is unknown.
    progress: Option<f32>,
    error: Option<SharedString>,
}

impl VideoSpeed {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let speed = cx.new(|cx| {
            InputState::new(window, cx)
                .mask_pattern(MaskPattern::Number { separator: None, fraction: Some(2) })
                .default_value("2")
                .min(-MAX_SPEED)
                .max(MAX_SPEED)
                .step_by(|value, action, _| speed_step(value, action))
        });
        // Redraw the output length as the speed changes.
        cx.observe(&speed, |_, _, cx| cx.notify()).detach();
        Self {
            input: None,
            duration: None,
            preview: None,
            speed,
            remove_audio: false,
            job: None,
            progress: None,
            error: None,
        }
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = picked.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |this, cx| this.load(path, cx));
        })
        .detach();
    }

    fn load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.job.is_some() {
            return;
        }
        self.input = Some(path.clone());
        self.duration = None;
        self.preview = None;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (preview, duration) = {
                let path = path.clone();
                cx.background_executor()
                    .spawn(async move { (extract_preview(&path), probe_duration(&path)) })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                // Ignore a late preview for a file the user already replaced.
                if this.input.as_ref() != Some(&path) {
                    return;
                }
                this.duration = duration;
                match preview {
                    Ok(png) => this.preview = Some(Arc::new(Image::from_bytes(ImageFormat::Png, png))),
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn process(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else { return };
        let Some(speed) = parse_speed(&self.speed.read(cx).value()) else {
            self.error = Some(
                format!("Speed must be {MIN_SPEED}x to {MAX_SPEED}x, or -{MIN_SPEED}x to -{MAX_SPEED}x to slow down.").into(),
            );
            cx.notify();
            return;
        };
        let rate = playback_rate(speed);
        let remove_audio = self.remove_audio;
        let stem = input.file_stem().unwrap_or_default().to_string_lossy();
        let dir = input.parent().unwrap_or(Path::new("."));
        let save = cx.prompt_for_new_path(dir, Some(&format!("{stem}_{speed}x.mp4")));

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(output))) = save.await else { return };
            let (job, cancel) = smol::channel::bounded(1);
            let total = this
                .update(cx, |this, cx| {
                    this.job = Some(job);
                    this.progress = this.duration.map(|_| 0.);
                    this.error = None;
                    cx.notify();
                    this.duration.map(|duration| duration / rate)
                })
                .ok()
                .flatten();
            let result = speed_up(&input, &output, rate, remove_audio, cancel, |seconds| {
                if let Some(total) = total {
                    let _ = this.update(cx, |this, cx| {
                        this.progress = Some((seconds / total * 100.) as f32);
                        cx.notify();
                    });
                }
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let cancelled = this.job.take().is_some_and(|job| job.is_closed());
                match result {
                    Ok(()) => window.push_notification(Notification::success(format!("Saved {}", output.display())), cx),
                    // The progress bar disappearing already says so.
                    Err(_) if cancelled => {}
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for VideoSpeed {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.job.is_some();
        let loading = self.input.is_some() && self.preview.is_none() && self.error.is_none();
        let theme = cx.theme();
        // Source length, and the output length once the speed is valid: "0:12 → 0:06".
        let lengths = self.duration.map(|duration| match parse_speed(&self.speed.read(cx).value()) {
            Some(speed) => format!("{} → {}", clock(duration), clock(duration / playback_rate(speed))),
            None => clock(duration),
        });

        div()
            .v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(div().text_sm().font_medium().child("Video Speed")))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_6()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(200.))
                            .rounded(theme.radius_lg)
                            .overflow_hidden()
                            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent))
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                if let Some(path) = paths.paths().first() {
                                    this.load(path.clone(), cx);
                                }
                            }))
                            .map(|this| match &self.preview {
                                // Absolute, so a short window letterboxes the frame instead of cropping it.
                                Some(image) => this.relative().bg(gpui_kit::black()).child(
                                    img(image.clone()).absolute().size_full().object_fit(ObjectFit::Contain),
                                ),
                                None => this.child(
                                    Empty::new()
                                        .size_full()
                                        .border_1()
                                        .rounded(theme.radius_lg)
                                        .header(
                                            EmptyHeader::new()
                                                .media(
                                                    EmptyMedia::new()
                                                        .with_variant(EmptyMediaVariant::Icon)
                                                        .size_12()
                                                        .rounded_full()
                                                        .text_xl()
                                                        .map(|media| {
                                                            if loading {
                                                                media.child(Spinner::new())
                                                            } else {
                                                                media.child(Icon::new(IconName::Inbox))
                                                            }
                                                        }),
                                                )
                                                .title(EmptyTitle::new().child(if loading {
                                                    "Loading preview"
                                                } else {
                                                    "Drop a video here"
                                                }))
                                                .when(!loading, |header| {
                                                    header.description(
                                                        EmptyDescription::new().child("MP4, MOV, MKV and anything else ffmpeg reads."),
                                                    )
                                                }),
                                        )
                                        .when(self.input.is_none(), |empty| {
                                            empty.content(
                                                EmptyContent::new().child(
                                                    Button::new("choose")
                                                        .outline()
                                                        .label("Choose video")
                                                        .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
                                                ),
                                            )
                                        }),
                                ),
                            }),
                    )
                    .when_some(self.input.as_ref(), |this, path| {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        this.child(
                            div()
                                .h_flex()
                                .gap_3()
                                .child(
                                    div()
                                        .flex()
                                        .flex_shrink_0()
                                        .size_9()
                                        .items_center()
                                        .justify_center()
                                        .rounded(theme.radius)
                                        .bg(theme.muted)
                                        .child(Icon::new(IconName::Play).text_color(theme.muted_foreground)),
                                )
                                .child(
                                    div()
                                        .v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Label::new(name).text_sm().font_medium().truncate())
                                        .when_some(lengths, |this, lengths| {
                                            this.child(div().text_xs().text_color(theme.muted_foreground).child(lengths))
                                        }),
                                )
                                .child(
                                    Button::new("replace")
                                        .ghost()
                                        .small()
                                        .label("Replace")
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
                                ),
                        )
                    })
                    .child(
                        div()
                            .v_flex()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .child(setting(
                                "Speed",
                                "Negative slows it down: -2x is half speed.",
                                NumberInput::new(&self.speed).suffix("x").w(px(132.)).disabled(busy),
                                cx,
                            ))
                            .child(div().h_px().bg(theme.border))
                            .child(setting(
                                "Remove audio",
                                "Export a silent video.",
                                Switch::new("remove-audio")
                                    .accessibility_label("Remove audio")
                                    .checked(self.remove_audio)
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.remove_audio = *checked;
                                        cx.notify();
                                    })),
                                cx,
                            )),
                    )
                    .when_some(self.error.clone(), |this, error| this.child(Alert::error("error", error)))
                    .map(|this| match &self.job {
                        Some(job) => {
                            let job = job.clone();
                            this.child(
                                div()
                                    .h_flex()
                                    .gap_4()
                                    .h_10()
                                    .child(
                                        div()
                                            .v_flex()
                                            .flex_1()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .h_flex()
                                                    .justify_between()
                                                    .text_xs()
                                                    .child(div().font_medium().child("Exporting…"))
                                                    .when_some(self.progress, |this, progress| {
                                                        this.child(
                                                            div()
                                                                .text_color(theme.muted_foreground)
                                                                .child(format!("{}%", progress.min(100.) as u32)),
                                                        )
                                                    }),
                                            )
                                            .child(
                                                Progress::new("progress")
                                                    .value(self.progress.unwrap_or(0.))
                                                    .loading(self.progress.is_none()),
                                            ),
                                    )
                                    .child(Button::new("cancel").outline().label("Cancel").on_click(move |_, _, _| {
                                        job.close();
                                    })),
                            )
                        }
                        None => this.child(
                            Button::new("process")
                                .primary()
                                .large()
                                .w_full()
                                .label("Export…")
                                .disabled(self.input.is_none())
                                .on_click(cx.listener(|this, _, window, cx| this.process(window, cx))),
                        ),
                    }),
            )
    }
}

/// A settings row: title and hint on the left, `control` on the right.
fn setting(title: &'static str, hint: &'static str, control: impl IntoElement, cx: &App) -> Div {
    div()
        .h_flex()
        .gap_4()
        .px_4()
        .py_3()
        .child(
            div()
                .v_flex()
                .flex_1()
                .gap_0p5()
                .child(div().text_sm().font_medium().child(title))
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint)),
        )
        .child(control)
}

/// Seconds as m:ss, or h:mm:ss past an hour.
fn clock(seconds: f64) -> String {
    let s = seconds.round() as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

/// A speed the user typed: 1.1 to 100 speeds up, -1.1 to -100 slows down.
fn parse_speed(text: &str) -> Option<f64> {
    text.trim().parse().ok().filter(|speed: &f64| (MIN_SPEED..=MAX_SPEED).contains(&speed.abs()))
}

/// Playback rate for a speed: 2x -> 2.0, -2x -> 0.5.
fn playback_rate(speed: f64) -> f64 {
    if speed < 0. { -1. / speed } else { speed }
}

fn speed_step(value: f64, action: StepAction) -> f64 {
    let magnitude = value.abs();
    let toward_zero = value != 0. && (value > 0.) == (action == StepAction::Decrement);
    // Jump over the gap between -1.1x and 1.1x, which holds no valid speeds.
    if toward_zero && magnitude <= MIN_SPEED {
        return magnitude + MIN_SPEED;
    }
    if magnitude < MIN_SPEED {
        return MIN_SPEED - magnitude;
    }
    // Leaving a boundary toward zero takes the smaller step, e.g. 2 -> 1.9, 10 -> 9.
    let magnitude = if toward_zero { magnitude - 1e-9 } else { magnitude };
    if magnitude < 2. { 0.1 } else if magnitude < 10. { 1. } else { 10. }
}

/// ffmpeg's atempo accepts 0.5 to 100, so slower rates chain halvings.
fn atempo(mut rate: f64) -> String {
    let mut filters = Vec::new();
    while rate < 0.5 {
        filters.push("atempo=0.5".to_string());
        rate /= 0.5;
    }
    filters.push(format!("atempo={rate}"));
    filters.join(",")
}

// ponytail: expects ffmpeg/ffprobe on PATH; a Finder-launched macOS .app won't see Homebrew's PATH, bundle ffmpeg when packaging.
fn tool(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000); // CREATE_NO_WINDOW
    command
}

fn ffmpeg() -> Command {
    let mut command = tool("ffmpeg");
    command.args(["-hide_banner", "-nostdin", "-v", "error"]);
    command
}

fn spawn_error(err: std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "ffmpeg not found. Install it and make sure it is on your PATH.".into(),
        _ => err.to_string(),
    }
}

fn last_line(stderr: &str) -> String {
    stderr.trim().lines().last().unwrap_or("ffmpeg failed").to_string()
}

fn run(command: &mut Command) -> Result<Vec<u8>, String> {
    let output = command.output().map_err(spawn_error)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(last_line(&String::from_utf8_lossy(&output.stderr)))
}

/// A PNG of the frame at 1s, or the first frame for clips shorter than that.
fn extract_preview(input: &Path) -> Result<Vec<u8>, String> {
    for seek in ["1", "0"] {
        let png = run(ffmpeg().args(["-ss", seek, "-i"]).arg(input).args(["-frames:v", "1", "-c:v", "png", "-f", "image2pipe", "pipe:1"]))?;
        if !png.is_empty() {
            return Ok(png);
        }
    }
    Err("Couldn't read a frame from this file.".into())
}

fn probe_duration(input: &Path) -> Option<f64> {
    let out = run(tool("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(input)).ok()?;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

/// Runs ffmpeg at `rate` (2.0 doubles speed, 0.5 halves it), reporting output seconds written. Closing `cancel`'s sender kills ffmpeg and deletes the partial output.
async fn speed_up(
    input: &Path,
    output: &Path,
    rate: f64,
    remove_audio: bool,
    cancel: Receiver<()>,
    mut on_progress: impl FnMut(f64),
) -> Result<(), String> {
    // Cancelling deletes `output`, so never let it be the source file.
    if std::fs::canonicalize(output).is_ok_and(|output| std::fs::canonicalize(input).is_ok_and(|input| input == output)) {
        return Err("Save to a different file than the original.".into());
    }
    let mut command = ffmpeg();
    command.args(["-y", "-progress", "pipe:1", "-i"]).arg(input).arg("-filter:v").arg(format!("setpts=PTS/{rate}"));
    if remove_audio {
        command.arg("-an");
    } else {
        command.arg("-filter:a").arg(atempo(rate));
    }
    command.arg(output);

    let mut child = smol::process::Command::from(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(spawn_error)?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut errors = String::new();
    let finished = async {
        // ffmpeg writes progress as key=value lines, e.g. `out_time_us=1500000`.
        let progress = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next().await {
                if let Some(Ok(micros)) = line.strip_prefix("out_time_us=").map(str::parse::<f64>) {
                    on_progress(micros / 1e6);
                }
            }
        };
        // Drain stderr alongside, or a chatty ffmpeg fills the pipe and stalls.
        let _ = future::zip(progress, stderr.read_to_string(&mut errors)).await;
        true
    };
    let cancelled = async {
        let _ = cancel.recv().await;
        false
    };
    if !future::or(finished, cancelled).await {
        let _ = child.kill();
        let _ = child.status().await;
        let _ = std::fs::remove_file(output);
        return Err("Cancelled.".into());
    }
    match child.status().await {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(last_line(&errors)),
        Err(err) => Err(err.to_string()),
    }
}

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions { title: Some("Video Speed".into()), ..TitleBar::title_bar_options() }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(560.), px(620.)), cx))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| VideoSpeed::new(window, cx)))
            .expect("failed to open window");
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Command, StepAction, atempo, clock, extract_preview, ffmpeg, parse_speed, playback_rate, probe_duration, run, speed_step,
        speed_up,
    };

    #[test]
    fn speeds_up_a_clip() {
        assert_eq!(parse_speed("2"), Some(2.));
        assert_eq!(parse_speed("1.1"), Some(1.1));
        assert_eq!(parse_speed("100"), Some(100.));
        assert_eq!(parse_speed("-2"), Some(-2.));
        assert_eq!(parse_speed("-100"), Some(-100.));
        assert_eq!(parse_speed("1"), None);
        assert_eq!(parse_speed("-1"), None);
        assert_eq!(parse_speed("0.5"), None);
        assert_eq!(parse_speed("100.5"), None);
        assert_eq!(parse_speed("-100.5"), None);
        assert_eq!(parse_speed("fast"), None);
        assert_eq!(playback_rate(-4.), 0.25);
        assert_eq!(playback_rate(3.), 3.);
        assert_eq!(atempo(2.), "atempo=2");
        assert_eq!(atempo(0.25), "atempo=0.5,atempo=0.5");
        assert_eq!(atempo(0.01).split(',').count(), 7);
        assert_eq!(clock(5.4), "0:05");
        assert_eq!(clock(125.), "2:05");
        assert_eq!(clock(3725.), "1:02:05");

        use StepAction::{Decrement as Down, Increment as Up};
        for (value, action, expected) in [
            (1.1, Down, -1.1),
            (-1.1, Up, 1.1),
            (1.1, Up, 1.2),
            (-1.1, Down, -1.2),
            (2., Down, 1.9),
            (-2., Up, -1.9),
            (10., Down, 9.),
            (10., Up, 20.),
            (0., Down, -1.1),
            (0., Up, 1.1),
        ] {
            let step = speed_step(value, action);
            let next = if action == Up { value + step } else { value - step };
            assert!((next - expected).abs() < 1e-6, "{value} {action:?} went to {next}");
        }

        let dir = std::env::temp_dir().join(format!("video-speed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        run(ffmpeg().args(["-y", "-f", "lavfi", "-i", "testsrc=duration=4:rate=30", "-f", "lavfi", "-i", "sine=duration=4", "-shortest"]).arg(&input)).unwrap();

        assert!(extract_preview(&input).unwrap().starts_with(b"\x89PNG"));
        assert!(extract_preview(&dir.join("missing.mp4")).is_err());
        assert!((probe_duration(&input).unwrap() - 4.).abs() < 0.1);

        // Holding the sender keeps the job running.
        let (_job, cancel) = smol::channel::bounded(1);
        for (rate, remove_audio) in [(2., false), (100., true), (0.25, false)] {
            let output = dir.join(format!("out_{rate}.mp4"));
            let (expected, tolerance) = (4. / rate, 0.3 + 0.05 * 4. / rate);
            let mut written = 0.;
            smol::block_on(speed_up(&input, &output, rate, remove_audio, cancel.clone(), |s| written = s)).unwrap();
            assert!((written - expected).abs() < tolerance, "{rate} rate reported {written}s");
            let probe = Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type", "-of", "csv=p=0"])
                .arg(&output)
                .output()
                .unwrap();
            let probe = String::from_utf8_lossy(&probe.stdout);
            let duration: f64 = probe.lines().last().unwrap().parse().unwrap();
            assert!((duration - expected).abs() < tolerance, "{rate} rate gave {duration}s");
            assert_eq!(probe.contains("audio"), !remove_audio);
        }

        // Saving over the source is refused, and the source survives.
        assert!(smol::block_on(speed_up(&input, &input, 2., false, cancel.clone(), |_| {})).is_err());
        assert!(input.exists());

        // A closed channel kills ffmpeg and removes the partial output.
        let (job, cancel) = smol::channel::bounded(1);
        job.close();
        let output = dir.join("cancelled.mp4");
        let result = smol::block_on(speed_up(&input, &output, 2., false, cancel, |_| {}));
        assert_eq!(result, Err("Cancelled.".into()));
        assert!(!output.exists());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
