// GNU AGPL v3 License

use super::Video;
use anyhow::Result;
use fastrand::Rng;
use std::cmp;

/// A clip in a video.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clip {
    /// Start in microseconds.
    pub(crate) start: i64,
    /// End in microseconds.
    pub(crate) end: i64,
    /// "Priority" of the clip, determined by relatively volume
    /// and image motion.
    pub(crate) priority: f64,
}

impl Clip {
    pub(crate) fn len(self) -> i64 {
        self.end - self.start + 1
    }
}

/// From a `Video`, determine what parts should be clipped.
pub(crate) fn clip_video(
    video: &Video,
    silence_threshold: f64,
    silence_time: i64,
    silence_time_degradation: f64,
) -> Result<Vec<Clip>> {
    // clip up the video
    let clips = Clipset::of(
        video,
        silence_threshold,
        silence_time,
        silence_time_degradation,
    )?
    .into_clips();

    // seed fastrand using the system RNG
    let mut seed = 0u64;
    getrandom::getrandom(bytemuck::bytes_of_mut(&mut seed))?;
    let rng = Rng::with_seed(seed);

    clips
        .into_iter()
        .map(|c| c.prioritize(video, &rng))
        .collect()
}

#[derive(Debug, Clone)]
struct Clipset {
    // invariant: contains at least 1 clip
    clips: Vec<UnprioritizedClip>,
}

impl Clipset {
    /// Create a new, empty clipset.
    fn new(start: i64, end: i64) -> Self {
        Self {
            clips: vec![UnprioritizedClip {
                start,
                end,
                relatively_silent: true,
            }],
        }
    }

    fn into_clips(self) -> Vec<UnprioritizedClip> {
        self.clips
    }

    /// Split the clipset at the given time.
    fn split(&mut self, time: i64, silent: bool) -> Result<()> {
        // look for the first clip that contains this time
        let clip_index = self
            .clips
            .iter()
            .rposition(|c| c.contains(time))
            .ok_or_else(|| anyhow::anyhow!("time {} is not in any clip", time))?;

        // get the two clips that will replace this clip
        let clip = self.clips[clip_index];
        let left = UnprioritizedClip {
            start: clip.start,
            end: time - 1,
            relatively_silent: clip.relatively_silent,
        };
        let right = UnprioritizedClip {
            start: time,
            end: clip.end,
            relatively_silent: silent,
        };

        // splice it into the clipset
        self.clips.splice(clip_index..=clip_index, [left, right]);

        Ok(())
    }

    /// Get a clipset for a video.
    fn of(
        video: &Video,
        silence_threshold: f64,
        silence_time: i64,
        silence_time_degradation: f64,
    ) -> Result<Self> {
        let mut clipset = Self::initial_clipset(
            video,
            silence_threshold,
            silence_time,
            silence_time_degradation,
        )?;
        clipset.merge_and_split_clips()?;
        Ok(clipset)
    }

    /// Get the initial set of clips for a video.
    fn initial_clipset(
        video: &Video,
        silence_threshold: f64,
        silence_time: i64,
        silence_time_degradation: f64,
    ) -> Result<Self> {
        // get the mean and standard deviation of the volume
        let num_volumes = video.volumes.len() as f64;
        let mean_volume = video.volumes.iter().map(|v| v.max_amplitude).sum::<f64>() / num_volumes;
        let standard_deviation = {
            let variance = video
                .volumes
                .iter()
                .map(|v| (v.max_amplitude - mean_volume).powi(2))
                .sum::<f64>()
                / num_volumes;
            variance.sqrt()
        };

        // get the highest and lowest time in the video
        let (lo, hi) = video
            .volumes
            .iter()
            .map(|v| v.microseconds)
            .chain(video.frames.iter().map(|f| f.microseconds))
            .fold((i64::MAX, i64::MIN), |(lo, hi), t| {
                (cmp::min(lo, t), cmp::max(hi, t))
            });

        // initialize the clipset
        let mut clipset = Clipset::new(lo, hi);

        // find gaps of silence where:
        // - the volume is more than a standard deviation below the main volume
        // - (or silence_threshold below the devation)
        // - the gap is longer than silence_time
        // - (for every second not in silence_time, the gap is divided by silence_time_degradation)
        let mut first_volume_time = None;
        let mut last_volume_time: Option<i64> = None;
        let mut current_silence_time = silence_time;
        let threshold = mean_volume - (silence_threshold * standard_deviation);

        for volume in video.volumes.iter() {
            if volume.max_amplitude < threshold {
                // this is relative silence
                //
                // if we're current_silence_time away from the last volume time,
                // add a clip to the clipset
                if let Some(lvt) = last_volume_time {
                    let diff = volume.microseconds - lvt;
                    if diff > current_silence_time {
                        first_volume_time = None;
                        last_volume_time = None;
                        clipset.split(volume.microseconds, true)?;
                        current_silence_time = silence_time;
                    }
                }
            } else {
                // this is relative noise
                let first_volume_time = match first_volume_time {
                    Some(t) => t,
                    None => {
                        first_volume_time = Some(volume.microseconds);
                        clipset.split(volume.microseconds, false)?;
                        volume.microseconds
                    }
                };
                last_volume_time = Some(volume.microseconds);

                // if we're 1 second away from the first volume time,
                // decrease the standards for the next clip
                let diff = volume.microseconds - first_volume_time;
                if diff >= 1_000_000 {
                    current_silence_time =
                        (current_silence_time as f64 / silence_time_degradation) as i64;
                }
            }
        }

        Ok(clipset)
    }

    /// Merge small clips together and split up large silent clips.
    fn merge_and_split_clips(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.clips.len() {
            // if this clip and the next one add up to less than 750 ms,
            // merge them together
            // also merge if this clip is exceptionally short
            if i + 1 < self.clips.len() {
                let next_clip = self.clips[i + 1];
                if self.clips[i].len() + next_clip.len() < 750_000 || self.clips[i].len() < 100_000
                {
                    self.clips[i].merge_from(next_clip);
                    self.clips.remove(i + 1);
                }
            } else if self.clips[i].len() > 5_000_000 && self.clips[i].relatively_silent {
                // exceptionally long clip, split it up
                self.split(self.clips[i].start + 5_000_000, true)?;
            }

            i += 1;
        }

        Ok(())
    }
}

/// Range is inclusive.
#[derive(Debug, Clone, Copy)]
struct UnprioritizedClip {
    start: i64,
    end: i64,
    relatively_silent: bool,
}

impl UnprioritizedClip {
    fn len(self) -> i64 {
        self.end - self.start + 1
    }

    fn contains(&self, time: i64) -> bool {
        time >= self.start && time <= self.end
    }

    fn merge_from(&mut self, other: UnprioritizedClip) {
        self.end = other.end;
        if self.len() < other.len() {
            self.relatively_silent = other.relatively_silent;
        }
    }

    fn prioritize(self, video: &Video, rng: &Rng) -> Result<Clip> {
        let Self { start, end, .. } = self;

        // priority is determined by average volume
        let mut priority = video
            .volumes
            .iter()
            .filter(|v| self.contains(v.microseconds))
            .map(|v| v.max_amplitude)
            .sum::<f64>()
            / video.volumes.len() as f64;

        // add some noise to the priority
        priority *= 1.0 + (rng.f64() * 0.2) - 0.15;

        Ok(Clip {
            start,
            end,
            priority,
        })
    }
}
