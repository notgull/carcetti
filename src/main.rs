// AGPL v3 License

// we need unsafe code for ffmpeg logging, but aside from that
// we can go without
#![deny(unsafe_code)]

use anyhow::{anyhow, Result};
use std::{
    collections::HashSet,
    env, fs, mem,
    path::{Path, PathBuf},
    process::{self, Stdio},
    sync::{Arc, RwLock},
    time::Duration,
};
use tempdir::TempDir;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    spawn,
    sync::{broadcast, mpsc},
    task::{JoinError, JoinHandle},
    time::interval,
};
use video::{Clip, Video};

mod log;
mod melt;
mod sort_clips;
mod tempdir;
mod ui;
mod video;
mod video_info;

fn main() {
    // initialization routines go here
    ffmpeg::init().expect("failed to initialize ffmpeg");
    log::register_ffmpeg_logger();

    // spawn the tokio runtime
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Unable to start the Tokio runtime: {:?}", e);
            eprintln!("Cannot continue, exiting...");
            process::exit(1);
        }
    };

    // run the main function
    if let Err(e) = runtime.block_on(entry()) {
        eprintln!("Encountered a fatal error: {:?}", e);

        // if it's a join panic, resume it
        if let Ok(jh) = e.downcast::<JoinError>() {
            if let Ok(pn) = jh.try_into_panic() {
                match pn.downcast::<String>() {
                    Ok(s) => eprintln!("Panic message: {}", s),
                    Err(pn) => {
                        if let Ok(s) = pn.downcast::<&'static str>() {
                            eprintln!("Panic message: {}", s);
                        }
                    }
                }
            }
        }

        process::exit(1);
    }
}

/// Intended to wrap the real main function (`processing`) with
/// a terminal UI.
async fn entry() -> Result<()> {
    // spawn the terminal UI thread
    let (mut send_data, recv_data) = mpsc::channel(10);
    let (send_ui, mut recv_ui) = broadcast::channel(16);

    let sd_clone = send_data.clone();
    tracing::info!("Spawning UI thread...");
    let ui_thread = tokio::spawn(async move {
        ui::ui_thread(recv_data, sd_clone, send_ui)
            .await
            .map_err(|e| {
                tracing::error!("UI thread failed: {}", e);
                e
            })
    });

    let res = processing(&mut send_data, &mut recv_ui).await;
    tracing::info!("Reached end of processing execution");
    if let Err(ref err) = res {
        tracing::error!("Processing failed: {}", err);
    }

    // send the stop signals
    // even if the main system failed, we should let the UI thread
    // stop gracefully to avoid corrupting the current
    // terminal env
    let _ = send_data.send(ui::UiDirective::Stop).await;

    ui_thread.await??;

    Ok(())
}

async fn processing(
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    recv_ui: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<()> {
    // open a temporary directory
    let tempdir = tempdir::TempDir::new().await?;

    // open the input file using libav
    let input_file = env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow!("Please provide an input file"))?;
    tracing::info!("Opening file {:?}", input_file);

    // create an output file to store the audio
    let audio_output_file = tempdir.path().join("video-audio.wav");

    let video_data = fetch_video_info(
        input_file.clone().into(),
        send_data,
        recv_ui,
        audio_output_file,
    )
    .await?;
    tracing::info!("{:?}", video_data);
    tracing::info!(
        "{} motion frames, {} audio frames",
        video_data.frame_motion.len(),
        video_data.audio_volume.len()
    );
    tracing::info!(
        "Average Peak Amplitude: {}",
        video_data
            .audio_volume
            .iter()
            .map(|av| av.average_volume)
            .sum::<f64>()
            / video_data.audio_volume.len() as f64
    );

    // make clips for the video
    let video: Arc<video::Video> = Arc::new(video_data.into());
    let mut video_config = VideoConfig {
        silence_threshold: 1.0,
        silence_degrade: 1.1,
        silence_time: 1_000_000,
        time_limit: 15_000_000,
    };

    let process_video = loop {
        let clips = make_video_clips(&video, &video_config, send_data, recv_ui).await?;
        tracing::info!("{} clips", clips.len());
        let clips: Arc<[Clip]> = clips.into_boxed_slice().into();

        // sort the clips
        let valid_clip_indices =
            sort_video_clips(&clips, &video_config, send_data, recv_ui).await?;
        tracing::info!("Retained {} clips", valid_clip_indices.len());
        let indices = Arc::new(RwLock::new(valid_clip_indices));

        // send the clips to the UI thread
        send_data
            .send(ui::UiDirective::Clips {
                clips: clips.clone(),
                indices: indices.clone(),
                config: video_config.clone(),
            })
            .await?;

        // wait for a UI response
        let ui_response = recv_ui.recv().await?;

        match ui_response {
            ui::UiMessage::Halt => break None,
            ui::UiMessage::RerunVideoProcess(vc) => {
                video_config = vc;
            }
            ui::UiMessage::GoForIt => break Some((clips, indices)),
        }
    };

    if let Some((clips, used_indices)) = process_video {
        tracing::info!("Processing video...");
        let video_path = make_melt_xml(
            video,
            input_file.into(),
            clips,
            used_indices,
            &tempdir,
            send_data,
        )
        .await?;

        run_melt(video_path, &tempdir, send_data, recv_ui).await?;
    }

    // destroy the tempdir
    if env::args().any(|a| a == "--keep-temp") {
        tracing::info!("Keeping temporary directory");
        mem::forget(tempdir);
    } else {
        tracing::info!("Removing temporary directory");
        tempdir.delete().await?;
    }

    Ok(())
}

async fn make_melt_xml(
    video: Arc<Video>,
    input_file: PathBuf,
    clips: Arc<[Clip]>,
    used_indices: Arc<RwLock<HashSet<usize>>>,
    tempdir: &TempDir,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
) -> Result<PathBuf> {
    const SOURCE_NAME: &str = "source";
    const MAIN_PLAYLIST_NAME: &str = "main_video";
    const MAIN_TRACTOR_NAME: &str = "main_tractor";

    let _ui_guard = spawn_ellipses_task(send_data, "Creating video configuration");

    // begin running MELT
    let video_path = env::args_os()
        .nth(2)
        .or_else(|| {
            let vd = dirs::video_dir()?;
            Some(vd.join("video.webm").into())
        })
        .ok_or_else(|| anyhow!("Please provide a video output path"))?;
    let melt_path = tempdir.path().join("assembly.xml");

    let indices = used_indices.read().unwrap();
    let duration = clips
        .iter()
        .enumerate()
        .filter(|(i, _)| indices.contains(i))
        .map(|(_, c)| c.len())
        .sum();
    let duration = microseconds_to_frames(duration, video.fps);
    let mut melt = melt::Melt::new(&melt_path, video_path.as_ref(), &video, duration)?;

    // add the source video as a producer
    melt.producer(&input_file, SOURCE_NAME)?;

    // add the clips into a giant playlist
    melt.playlist(
        clips
            .iter()
            .enumerate()
            .filter(|(i, _)| indices.contains(i))
            .map(|(_, clip)| {
                // determine the start and end
                let start = microseconds_to_frames(clip.start, video.fps);
                let end = microseconds_to_frames(clip.end, video.fps);

                melt::PlaylistItem::Entry {
                    id: SOURCE_NAME,
                    start,
                    end,
                }
            }),
        MAIN_PLAYLIST_NAME,
    )?;
    drop(indices);

    // TODO: music

    // create a tractor made up of the playlist
    melt.tractor(Some(MAIN_PLAYLIST_NAME), MAIN_TRACTOR_NAME)?;

    // now, render the video to the MLT file
    melt.write_file(MAIN_TRACTOR_NAME).await?;

    Ok(melt_path)
}

async fn run_melt(
    video_file: PathBuf,
    tempdir: &TempDir,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<()> {
    let _ui_guard = spawn_ellipses_task(send_data, "Running MELT (this may take a while)");
    let mut melt = Command::new("melt")
        .current_dir(tempdir.path())
        .arg(video_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = BufReader::new(melt.stdout.take().unwrap());
    let mut stderr = BufReader::new(melt.stderr.take().unwrap());
    let mut outbuffer = String::new();
    let mut errbuffer = String::new();

    // BEGIN PROCESSING THE MELT OUTPUT
    tracing::info!("Starting MELT...");
    loop {
        tokio::select! {
            res = melt.wait() => {
                res?;
                break;
            },
            msg = ui_data.recv() => {
                if let ui::UiMessage::Halt = msg? {
                    melt.kill().await?;
                    break;
                }
            },
            res = stdout.read_line(&mut outbuffer) => {
                res?;
                tracing::info!("{}", &outbuffer);
                outbuffer.clear();
            },
            res = stderr.read_line(&mut errbuffer) => {
                res?;
                tracing::error!("{}", &errbuffer);
                errbuffer.clear();
            }
        }
    }

    // drain the buffers before we return
    while stdout.read_line(&mut outbuffer).await.is_ok() {
        if outbuffer.is_empty() {
            break;
        }
        tracing::info!("{}", &outbuffer);
        outbuffer.clear();
    }

    while stderr.read_line(&mut errbuffer).await.is_ok() {
        if errbuffer.is_empty() {
            break;
        }
        tracing::error!("{}", &errbuffer);
        errbuffer.clear();
    }

    Ok(())
}

async fn fetch_video_info(
    input_file: PathBuf,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
    audio_output: PathBuf,
) -> Result<video_info::VideoInfo> {
    // tell the UI thread that we're loading video info
    let _ui_guard = spawn_ellipses_task(send_data, "Processing video file");

    // get the video info
    let handle = tokio::task::spawn_blocking(move || {
        video_info::video_info(&input_file, audio_output).map_err(|e| {
            tracing::error!("Failed to parse video info: {}", e);
            e
        })
    });

    finish_task(handle, ui_data).await?
}

async fn make_video_clips(
    video: &Arc<Video>,
    video_config: &VideoConfig,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<Vec<Clip>> {
    let _ui_guard = spawn_ellipses_task(send_data, "Processing video into clips");

    let video = video.clone();
    let VideoConfig {
        silence_threshold,
        silence_time,
        silence_degrade,
        ..
    } = video_config.clone();

    let handle = tokio::task::spawn_blocking(move || {
        video
            .make_clips(silence_threshold, silence_time, silence_degrade)
            .map_err(|e| {
                tracing::error!("Failed to make clips: {}", e);
                e
            })
    });

    finish_task(handle, ui_data).await?
}

async fn sort_video_clips(
    clips: &Arc<[Clip]>,
    config: &VideoConfig,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<HashSet<usize>> {
    let _ui_guard = spawn_ellipses_task(send_data, "Sorting video clips into sequences");

    let clips = clips.clone();
    let time_limit = config.time_limit;
    let handle = tokio::task::spawn_blocking(move || sort_clips::sort_clips(&clips, time_limit));

    Ok(finish_task(handle, ui_data).await?)
}

/// Begin a message that has an ellipses after it.
fn spawn_ellipses_task(
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    text: &'static str,
) -> mpsc::Sender<()> {
    // spawn a task with a timer that sends a message to the UI thread
    let (stop_send, mut stop_recv) = mpsc::channel(1);

    // create a recurring timer for messaging
    let mut timer = interval(Duration::from_millis(300));

    // create a function that sends the message
    let mut dots = 2;
    let mut update = move || {
        dots += 1;
        if dots > 3 {
            dots = 1;
        }

        let mut buf = String::from(text);
        buf.reserve(dots);
        for _ in 0..dots {
            buf.push('.');
        }

        buf
    };

    let send_data = send_data.clone();

    // spawn a detached task with a timer
    tokio::spawn(async move {
        loop {
            // wait to be dropped or for the timer to fire
            tokio::select! {
                _ = timer.tick() => {
                    // send the message
                    let msg = update();
                    send_data.send(ui::UiDirective::DisplayText(msg)).await.ok();
                }
                _ = stop_recv.recv() => {
                    // we're done here
                    break;
                }
            }
        }

        anyhow::Ok(())
    });

    stop_send
}

/// Either complete a task defined by a `JoinHandle` or halt when the user requests.
async fn finish_task<J>(
    mut jh: JoinHandle<J>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<J> {
    loop {
        tokio::select! {
            data = &mut jh => {
                return Ok(data?);
            }
            msg = ui_data.recv() => {
                if let Ok(ui::UiMessage::Halt) = msg {
                    jh.abort();
                    return Err(anyhow!("User requested stop"));
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct VideoConfig {
    silence_threshold: f64,
    silence_time: i64,
    silence_degrade: f64,
    time_limit: i64,
}

#[inline]
fn microseconds_to_frames(microseconds: i64, fps: f64) -> i64 {
    (microseconds as f64 / 1_000_000.0 * fps).round() as i64
}
