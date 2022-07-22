// GNU AGPL v3 License

use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use ffmpeg::{
    decoder::{Audio as AudioDecoder, Video as VideoDecoder},
    encoder::Audio as AudioEncoder,
    format::{context::Input, output, Output, Pixel},
    frame::{Audio, Video},
    media::Type,
    packet::Packet,
    software::{
        resampling::Context as RContext,
        scaling::{Context, Flags},
    },
    ChannelLayout, Rational, Stream,
};
use image::RgbImage;
use std::{
    ffi::CString,
    fmt, mem,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    thread::{self, JoinHandle},
};

const EMIT_EVERY: usize = 1000;

mod sample;
pub(crate) struct VideoInfo {
    pub video_timebase: f64,
    pub audio_timebase: f64,
    pub frame_motion: Vec<ImageMotion>,
    pub audio_volume: Vec<AudioVolume>,
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
}

impl fmt::Debug for VideoInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoInfo")
            .field("video_timebase", &self.video_timebase)
            .field("audio_timebase", &self.audio_timebase)
            .finish_non_exhaustive()
    }
}

pub(crate) fn video_info(path: impl AsRef<Path>, audio_output: PathBuf) -> Result<VideoInfo> {
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
    let mut video_decoder = video.codec().decoder().video()?;
    let (width, height) = (video_decoder.width(), video_decoder.height());
    let fps = video_decoder
        .frame_rate()
        .ok_or_else(|| anyhow!("Could not get video frame rate"))?;
    let handle = spawn_video_thread(video_decoder, &recv_video_packets)?;
    video_handles.push(handle);

    // spawn a thread to encode audio packets and save them in our temp
    // dir
    let (send_encoded_audio_packets, recv_encoded_audio_packets) = bounded::<Audio>(10000);
    let mut audio_decoder = audio.codec().decoder().audio()?;
    let encoder_handle = spawn_audio_encoding_thread(
        &input_ctx,
        &mut audio_decoder,
        recv_encoded_audio_packets,
        audio_output,
    )
    .context("Spawning encoding thread")?;

    // spawn a thread to decode audio packets
    let (send_audio_packets, recv_audio_packets) = bounded::<Packet>(10000);
    let mut audio_handles = vec![];

    let handle = spawn_audio_thread(
        audio_decoder,
        &recv_audio_packets,
        send_encoded_audio_packets,
    )?;
    audio_handles.push(handle);

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
                tracing::error!("Audio calculation occurred: {:?}", e);
                vec![]
            }
        })
        .collect::<Vec<_>>();
    encoder_handle.join().map_err(|_| {
        tracing::error!("Audio encoding thread failed");
        anyhow!("Audio encoding thread failed")
    })??;

    tracing::info!("{} motions, {} volumes", motions.len(), volumes.len());

    // zip them together
    Ok(VideoInfo {
        video_timebase: rational_to_float(&video_timebase),
        audio_timebase: rational_to_float(&audio_timebase),
        frame_motion: motions,
        audio_volume: volumes,
        width,
        height,
        frame_rate: rational_to_float(&fps),
    })
}

fn spawn_video_thread(
    mut video_decoder: VideoDecoder,
    recv_video_packets: &Receiver<Packet>,
) -> Result<JoinHandle<Result<Vec<ImageMotion>>>> {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let id = ID.fetch_add(1, Relaxed);

    // create the decoder
    let recv_video_packets = recv_video_packets.clone();

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
    mut audio_decoder: AudioDecoder,
    recv_video_packets: &Receiver<Packet>,
    send_encoded_audio_packets: Sender<Audio>,
) -> Result<JoinHandle<Result<Vec<AudioVolume>>>> {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let id = ID.fetch_add(1, Relaxed);

    // create the decoder
    let recv_audio_packets = recv_video_packets.clone();

    Ok(thread::Builder::new()
        .name("audio-processor".to_string())
        .spawn(move || {
            let mut volumes = vec![];

            loop {
                match recv_audio_packets.recv() {
                    Ok(packet) => {
                        audio_decoder.send_packet(&packet)?;
                        handle_audio_decoding(
                            &mut audio_decoder,
                            &mut volumes,
                            &send_encoded_audio_packets,
                        )?;
                    }
                    Err(_) => {
                        // video channel is dropped, signalling time to end
                        audio_decoder.send_eof()?;
                        handle_audio_decoding(
                            &mut audio_decoder,
                            &mut volumes,
                            &send_encoded_audio_packets,
                        )?;
                        break;
                    }
                }
            }

            anyhow::Ok(volumes)
        })?)
}

fn spawn_audio_encoding_thread(
    input: &Input,
    decoder: &mut AudioDecoder,
    recv_encoded_audio_packets: Receiver<Audio>,
    output_path: PathBuf,
) -> Result<JoinHandle<Result<()>>> {
    // create the output file
    let mut out_ctx = output(&output_path).context("opening ctx")?;

    let global = out_ctx
        .format()
        .flags()
        .contains(ffmpeg::format::flag::Flags::GLOBAL_HEADER);

    // create an audio encoder
    let codec = ffmpeg::encoder::find(
        out_ctx
            .format()
            .codec(&output_path, ffmpeg::media::Type::Audio),
    )
    .ok_or_else(|| anyhow!("failed to find encoder"))?
    .audio()?;

    // create a stream, then start muxing on it
    let mut out_stream = out_ctx.add_stream(codec).context("add_stream")?;
    let mut encoder = out_stream.codec().encoder().audio()?;

    // clone the encoder as best as we can
    let channel_layout = codec
        .channel_layouts()
        .map(|cls| cls.best(decoder.channel_layout().channels()))
        .unwrap_or(ChannelLayout::STEREO);

    if global {
        encoder.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    encoder.set_sample_rate(decoder.sample_rate());
    encoder.set_channel_layout(channel_layout);
    encoder.set_channels(channel_layout.channels());
    encoder.set_format(
        codec
            .formats()
            .expect("unknown supported formats")
            .next()
            .unwrap(),
    );
    encoder.set_bit_rate(decoder.bit_rate());
    encoder.set_max_bit_rate(decoder.max_bit_rate());

    let mut encoder = encoder.open_as(codec).context("open_as")?;
    out_stream.set_parameters(&encoder);

    let in_time_base = decoder.time_base();
    let out_time_base = encoder.time_base();
    let out_index = out_stream.index();

    encoder.set_time_base((1, decoder.sample_rate() as i32));
    out_stream.set_time_base((1, decoder.sample_rate() as i32));

    out_ctx.set_metadata(input.metadata().to_owned());
    out_ctx.write_header().context("writing header")?;

    let mut context = RContext::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.sample_rate(),
        encoder.format(),
        encoder.channel_layout(),
        encoder.sample_rate(),
    )?;

    // spawn the transcoding thread
    Ok(thread::Builder::new()
        .name("transcoding-thread".to_string())
        .spawn(move || {
            let mut buffer = Audio::empty();

            while let Ok(audio) = recv_encoded_audio_packets.recv() {
                // resample the frame
                let mut more_frames = context.run(&audio, &mut buffer)?.is_some();

                // encode the frame
                loop {
                    encoder.send_frame(&buffer)?;
                    handle_audio_encoding(
                        &mut encoder,
                        &mut out_ctx,
                        &in_time_base,
                        &out_time_base,
                        out_index,
                    )?;

                    if more_frames {
                        more_frames = context.flush(&mut buffer)?.is_some();
                    } else {
                        break;
                    }
                }
            }

            encoder.send_eof()?;
            handle_audio_encoding(
                &mut encoder,
                &mut out_ctx,
                &in_time_base,
                &out_time_base,
                out_index,
            )?;

            out_ctx.write_trailer()?;

            anyhow::Ok(())
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
    pub(crate) average_volume: f64,
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
                .pts()
                .ok_or_else(|| anyhow!("Failed to get timestamp"))?,
        })
    }

    Ok(())
}

fn handle_audio_decoding(
    decoder: &mut AudioDecoder,
    volumes: &mut Vec<AudioVolume>,
    send_encoded_audio_packets: &Sender<Audio>,
) -> Result<()> {
    loop {
        let mut aframe = Audio::empty();
        if decoder.receive_frame(&mut aframe).is_err() {
            break;
        }

        let format = aframe.format();

        // iterate over every plane
        let peak_volume = (0..aframe.planes())
            .map(|plane| aframe.data(plane))
            .filter_map(|plane| plane.get(..aframe.samples()))
            .map(|plane| sample::decode_samples(plane, format))
            .filter_map(|samples| match samples {
                Ok(samples) => Some(samples),
                Err(e) => {
                    tracing::error!("Could not collect samples: {}", e);
                    None
                }
            })
            .flatten()
            .map(ordered_float::OrderedFloat)
            .max()
            .map(|v| v.0)
            .and_then(|v| {
                ordered_float::NotNan::new(v).ok().or_else(|| {
                    tracing::warn!("Encountered NaN max sample");
                    None
                })
            })
            .map_or(0.0, |v| v.into_inner());

        volumes.push(AudioVolume {
            timestamp: aframe
                .pts()
                .ok_or_else(|| anyhow!("Failed to get timestamp"))?,
            average_volume: peak_volume,
        });

        // send the audio packet to the encoder
        send_encoded_audio_packets
            .send(aframe)
            .map_err(|_| anyhow!("Failed to send audio packet to encoder"))?;
    }

    Ok(())
}

fn handle_audio_encoding(
    encoder: &mut AudioEncoder,
    out_ctx: &mut ffmpeg::format::context::Output,
    in_time_base: &Rational,
    out_time_base: &Rational,
    out_index: usize,
) -> Result<()> {
    let mut encoded = Packet::empty();

    while encoder.receive_packet(&mut encoded).is_ok() {
        encoded.set_stream(out_index);
        encoded.rescale_ts(*in_time_base, *out_time_base);
        encoded.write(out_ctx)?;
    }

    Ok(())
}

fn rational_to_float(rational: &Rational) -> f64 {
    rational.numerator() as f64 / rational.denominator() as f64
}
