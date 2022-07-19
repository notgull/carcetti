// GNU AGPL v3 License

use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, unbounded, Receiver};
use ffmpeg::{
    decoder::{Audio as AudioDecoder, Video as VideoDecoder},
    format::Pixel,
    frame::{Audio, Video},
    media::Type,
    packet::Packet,
    software::scaling::{Context, Flags},
    Rational, Stream,
};
use image::RgbImage;
use std::{
    ffi::CString,
    fmt, mem,
    ops::Deref,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    thread::{self, JoinHandle},
};

const EMIT_EVERY: usize = 50;

pub(crate) struct VideoInfo {
    pub video_timebase: f64,
    pub audio_timebase: f64,
    pub frame_motion: Vec<ImageMotion>,
    pub audio_volume: Vec<AudioVolume>,
}

impl fmt::Debug for VideoInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoInfo")
            .field("video_timebase", &self.video_timebase)
            .field("audio_timebase", &self.audio_timebase)
            .finish_non_exhaustive()
    }
}

pub(crate) fn video_info(path: impl AsRef<Path>) -> Result<VideoInfo> {
    let mut input_ctx = ffmpeg::format::input(path.as_ref())?;

    // open up basic structures
    let video = input_ctx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow!("No video stream found"))?;
    let audio = input_ctx
        .streams()
        .best(Type::Audio)
        .ok_or_else(|| anyhow!("No audio stream found"))?;
    let video_index = video.index();
    let audio_index = audio.index();

    let video_timebase = video.time_base();
    let audio_timebase = audio.time_base();

    tracing::info!("Opened decoders for audio and video");

    // set up multithreaded processing channels
    // note: you may notice a discarded attempt to multithread here. somehow,
    // libav doesn't like that, so we just split it into audio and video threads
    let (send_video_packets, recv_video_packets) = bounded::<Packet>(10000);

    let mut video_handles = vec![];

    // spawn a thread to decode video packets
    let handle = spawn_video_thread(&video, &recv_video_packets)?;
    video_handles.push(handle);

    let (send_audio_packets, recv_audio_packets) = bounded::<Packet>(10000);
    let mut audio_handles = vec![];

    let handle = spawn_audio_thread(&audio, &recv_audio_packets)?;
    audio_handles.push(handle);

    let mut current_video_packet = 0;
    let mut current_audio_packet = 0;

    // begin reading packets from the input file
    for (iter_index, res) in input_ctx.packets().enumerate() {
        if iter_index % EMIT_EVERY == 0 {
            tracing::info!("Processed {} packets", iter_index);
        }

        let (stream, packet) = res?;
        let index = stream.index();

        if index == video_index {
            if iter_index % EMIT_EVERY == 0 {
                tracing::info!("Processing video packet");
            }

            // send packet to decoder and start decoding
            send_video_packets.send(packet)?;
        } else if index == audio_index {
            if iter_index % EMIT_EVERY == 0 {
                tracing::info!("Processing audio packet");
            }

            send_audio_packets.send(packet)?;
        }
    }

    mem::drop((send_audio_packets, send_video_packets));

    // join the threads
    let mut motions = video_handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .flat_map(|res| match res {
            Ok(motions) => motions,
            Err(e) => {
                tracing::error!("Image motion calculation occurred: {:?}", e);
                vec![]
            }
        })
        .collect::<Vec<_>>();
    let mut volumes = audio_handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .flat_map(|res| match res {
            Ok(motions) => motions,
            Err(e) => {
                tracing::error!("Image motion calculation occurred: {:?}", e);
                vec![]
            }
        })
        .collect::<Vec<_>>();

    tracing::info!("{} motions, {} volumes", motions.len(), volumes.len());

    // zip them together
    Ok(VideoInfo {
        video_timebase: rational_to_float(&video_timebase),
        audio_timebase: rational_to_float(&audio_timebase),
        frame_motion: motions,
        audio_volume: volumes,
    })
}

fn spawn_video_thread(
    stream: &Stream,
    recv_video_packets: &Receiver<Packet>,
) -> Result<JoinHandle<Result<Vec<ImageMotion>>>> {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let id = ID.fetch_add(1, Relaxed);

    // create the decoder
    let recv_video_packets = recv_video_packets.clone();
    let mut video_decoder = stream.codec().decoder().video()?;

    let mut scaler = Context::get(
        video_decoder.format(),
        video_decoder.width(),
        video_decoder.height(),
        Pixel::RGB24,
        video_decoder.width(),
        video_decoder.height(),
        Flags::BILINEAR,
    )?;

    Ok(thread::Builder::new()
        .name("video-processor".to_string())
        .spawn(move || {
            let mut last_image = None;
            let mut motions = vec![];

            loop {
                match recv_video_packets.recv() {
                    Ok(packet) => {
                        video_decoder.send_packet(&packet)?;
                        handle_video_decoding(
                            &mut video_decoder,
                            &mut last_image,
                            &mut motions,
                            &mut scaler,
                        )?;
                    }
                    Err(_) => {
                        // video channel is dropped, signalling time to end
                        video_decoder.send_eof()?;
                        handle_video_decoding(
                            &mut video_decoder,
                            &mut last_image,
                            &mut motions,
                            &mut scaler,
                        )?;
                        break;
                    }
                }
            }

            anyhow::Ok(motions)
        })?)
}

fn spawn_audio_thread(
    stream: &Stream,
    recv_video_packets: &Receiver<Packet>,
) -> Result<JoinHandle<Result<Vec<AudioVolume>>>> {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let id = ID.fetch_add(1, Relaxed);

    // create the decoder
    let recv_audio_packets = recv_video_packets.clone();
    let mut audio_decoder = stream.codec().decoder().audio()?;

    Ok(thread::Builder::new()
        .name("audio-processor".to_string())
        .spawn(move || {
            let mut volumes = vec![];

            loop {
                match recv_audio_packets.recv() {
                    Ok(packet) => {
                        audio_decoder.send_packet(&packet)?;
                        handle_audio_decoding(&mut audio_decoder, &mut volumes)?;
                    }
                    Err(_) => {
                        // video channel is dropped, signalling time to end
                        audio_decoder.send_eof()?;
                        handle_audio_decoding(&mut audio_decoder, &mut volumes)?;
                        break;
                    }
                }
            }

            anyhow::Ok(volumes)
        })?)
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PacketIndex {
    packet: usize,
    frame: usize,
}

#[derive(Clone)]
pub(crate) struct ImageMotion {
    pub(crate) timestamp: i64,
}

#[derive(Clone)]
pub(crate) struct AudioVolume {
    pub(crate) timestamp: i64,
    pub(crate) average_volume: usize,
}

#[derive(Clone)]
pub(crate) struct Cframe {
    pub(crate) average_volume: usize,
    pub(crate) motion: ImageMotion,
}

fn handle_video_decoding(
    decoder: &mut VideoDecoder,
    last_image: &mut Option<RgbImage>,
    motions: &mut Vec<ImageMotion>,
    scaler: &mut Context,
) -> Result<()> {
    let mut vframe = Video::empty();

    while decoder.receive_frame(&mut vframe).is_ok() {
        // convert to RGB
        let mut rgb_frame = Video::empty();
        scaler.run(&vframe, &mut rgb_frame)?;
        let data = rgb_frame.data(0).to_vec();

        // create an RGB Image
        let image = RgbImage::from_vec(decoder.width(), decoder.height(), data)
            .ok_or_else(|| anyhow!("Failed to create RGB image"))?;

        // TODO: compare for motion vectors
        *last_image = Some(image);
        motions.push(ImageMotion {
            timestamp: vframe
                .timestamp()
                .ok_or_else(|| anyhow!("Failed to get timestamp"))?,
        })
    }

    Ok(())
}

fn handle_audio_decoding(decoder: &mut AudioDecoder, volumes: &mut Vec<AudioVolume>) -> Result<()> {
    let mut aframe = Audio::empty();

    while decoder.receive_frame(&mut aframe).is_ok() {
        let samples = aframe.data(0);

        let total_volume = samples
            .iter()
            .map(|a| *a as usize)
            .fold(0usize, |acc, x| acc.saturating_add(x));
        let average_volume = total_volume.saturating_div(samples.len());

        volumes.push(AudioVolume {
            timestamp: aframe
                .timestamp()
                .ok_or_else(|| anyhow!("Failed to get timestamp"))?,
            average_volume,
        });
    }

    Ok(())
}

fn rational_to_float(rational: &Rational) -> f64 {
    rational.numerator() as f64 / rational.denominator() as f64
}
