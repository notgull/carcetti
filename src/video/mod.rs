// GNU AGPL v3 License

mod clips;

use crate::video_info::VideoInfo;
use anyhow::Result;

pub(crate) use clips::Clip;

#[derive(Debug, Clone)]
pub(crate) struct Video {
    pub(crate) volumes: Vec<Volume>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: f64,
}

impl Video {
    pub(crate) fn make_clips(
        &self,
        silence_threshold: f64,
        silence_time: i64,
        silence_time_degradation: f64,
        borrow_factor: f64,
    ) -> Result<Vec<clips::Clip>> {
        clips::clip_video(
            self,
            silence_threshold,
            silence_time,
            silence_time_degradation,
            borrow_factor,
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Volume {
    pub(crate) microseconds: i64,
    pub(crate) max_amplitude: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub(crate) microseconds: i64,
}

impl From<VideoInfo> for Video {
    fn from(info: VideoInfo) -> Self {
        let VideoInfo {
            video_timebase,
            audio_timebase,
            frame_motion,
            audio_volume,
            width,
            height,
            frame_rate,
        } = info;

        Video {
            volumes: audio_volume
                .into_iter()
                .map(|av| Volume {
                    microseconds: pts_to_microseconds(av.timestamp, audio_timebase),
                    max_amplitude: av.average_volume,
                })
                .collect(),
            frames: frame_motion
                .into_iter()
                .map(|fm| Frame {
                    microseconds: pts_to_microseconds(fm.timestamp, video_timebase),
                })
                .collect(),
            width,
            height,
            fps: frame_rate,
        }
    }
}

#[inline]
fn pts_to_microseconds(pts: i64, time_base: f64) -> i64 {
    (pts as f64 * time_base * 1_000_000.0) as i64
}
