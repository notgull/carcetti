// GNU AGPL v3 License

use crate::video::Clip;
use std::collections::HashSet;

/// Take the clips, sort them, and determine which ones to activate.
pub(crate) fn sort_clips(clips: &[Clip], mut time_limit: i64) -> HashSet<usize> {
    // put the clips into a form we can sort, and then sort them
    let mut clips = clips.iter().copied().enumerate().collect::<Vec<_>>();
    clips.sort_unstable_by(|a, b| a.1.priority.partial_cmp(&b.1.priority).unwrap());

    // pop a set number of clips off of the end of the list, and
    // return their indices
    let mut out = HashSet::new();
    loop {
        if time_limit <= 0 {
            break;
        }
        let (i, clip) = clips.pop().unwrap();
        time_limit -= clip.len();
        out.insert(i);
    }
    out
}
