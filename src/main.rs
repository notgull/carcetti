// AGPL v3 License

use anyhow::{anyhow, Result};
use std::{
    env,
    path::{Path, PathBuf},
    process,
    time::Duration,
};
use tokio::{
    spawn,
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::interval,
};

mod ui;
mod video_info;

fn main() {
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
        eprintln!("Encountered a fatal error: {}", e);
        process::exit(1);
    }
}

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

    // open the input file using libav
    let input_file = env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow!("Please provide an input file"))?;
    tracing::info!("Opening file {:?}", input_file);

    let mut video_data = fetch_video_info(input_file.into(), &mut send_data, &mut recv_ui).await?;
    tracing::info!("{:?}", video_data);
    tracing::info!(
        "{} motion frames, {} audio frames",
        video_data.frame_motion.len(),
        video_data.audio_volume.len()
    );

    // send the stop signals
    let _ = send_data.send(ui::UiDirective::Stop).await;

    ui_thread.await??;

    Ok(())
}

async fn fetch_video_info(
    input_file: PathBuf,
    send_data: &mut mpsc::Sender<ui::UiDirective>,
    ui_data: &mut broadcast::Receiver<ui::UiMessage>,
) -> Result<video_info::VideoInfo> {
    // tell the UI thread that we're loading video info
    let _ui_guard = spawn_ellipses_task(send_data, "Processing video file");

    // get the video info
    let handle = tokio::task::spawn_blocking(move || {
        video_info::video_info(&input_file).map_err(|e| {
            tracing::error!("Failed to parse video info: {}", e);
            e
        })
    });

    finish_task(handle, ui_data).await?
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
