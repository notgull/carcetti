// GNU AGPL v3 License

use super::video::Video;
use anyhow::Result;
use camino::Utf8Path;
use quick_xml::{
    events::{attributes::Attribute, BytesDecl, BytesEnd, BytesStart, Event},
    Writer,
};
use std::{
    convert::TryInto,
    fs, io,
    path::Path,
};

mod playlist;
mod producer;
mod tractor;

/// An in-progress MELT input file.
pub(crate) struct Melt<'a> {
    path: &'a Utf8Path,
    output_path: &'a Utf8Path,
    events: Vec<Event<'a>>,
    video_size: (u32, u32),
    fps: f64,
    duration: i64,
}

impl<'a> Melt<'a> {
    /// Create a new Melt input file.
    pub(crate) fn new(
        path: &'a Path,
        output_path: &'a Path,
        video: &Video,
        duration: i64,
    ) -> Result<Self> {
        Ok(Self {
            path: path.try_into()?,
            output_path: output_path.try_into()?,
            events: Vec::new(),
            video_size: (video.width, video.height),
            fps: video.fps,
            duration,
        })
    }

    /// Add a new producer to the file.
    pub(crate) fn producer(&mut self, resource: &'a Path, id: &'a str) -> Result<()> {
        self.events
            .extend(producer::producer_xml(resource.try_into()?, id));
        Ok(())
    }

    /// Add a new playlist to the file.
    pub(crate) fn playlist(
        &mut self,
        entries: impl IntoIterator<Item = PlaylistItem<'a>>,
        id: &'a str,
    ) -> Result<()> {
        self.events
            .extend(playlist::playlist_xml(entries.into_iter(), id));
        Ok(())
    }

    /// Add a new tractor to the file.
    pub(crate) fn tractor(
        &mut self,
        multitrack: impl IntoIterator<Item = &'a str>,
        id: &'a str,
    ) -> Result<()> {
        self.events
            .extend(tractor::tractor_xml(multitrack.into_iter(), id));
        Ok(())
    }

    /// Convert the file to the final XML events.
    fn into_events(self, main_tractor: &'a str) -> impl Iterator<Item = Event<'a>> {
        const FRAME_RATE_DENOMINATOR: f64 = 1_000_000.0;

        // mlt opener and closer
        let root = self.path.parent().expect("no root directory");
        let opener = Event::Start(BytesStart::borrowed_name(b"mlt").with_attributes([
            Attribute {
                key: b"title",
                value: b"King of the Internet".as_slice().into(),
            },
            Attribute {
                key: b"producer",
                value: main_tractor.as_bytes().into(),
            },
            Attribute {
                key: b"root",
                value: root.as_str().as_bytes().into(),
            },
        ]));
        let closer = Event::End(BytesEnd::borrowed(b"mlt"));

        // profile description
        let numerator = (self.fps * FRAME_RATE_DENOMINATOR) as u64;
        let profile = Event::Empty(BytesStart::borrowed_name(b"profile").with_attributes([
            Attribute {
                key: b"width",
                value: self.video_size.0.to_string().as_bytes().into(),
            },
            Attribute {
                key: b"height",
                value: self.video_size.1.to_string().as_bytes().into(),
            },
            Attribute {
                key: b"frame_rate_num",
                value: numerator.to_string().as_bytes().into(),
            },
            Attribute {
                key: b"frame_rate_den",
                value: FRAME_RATE_DENOMINATOR.to_string().as_bytes().into(),
            },
        ]));

        // consumer for the end result
        let consumer = Event::Empty(
            BytesStart::borrowed_name(b"consumer").with_attributes([
                Attribute {
                    key: b"f",
                    value: self
                        .output_path
                        .extension()
                        .unwrap_or("webm")
                        .as_bytes()
                        .into(),
                },
                Attribute {
                    key: b"target",
                    value: self.output_path.as_str().as_bytes().into(),
                },
                Attribute {
                    key: b"in",
                    value: b"0".as_slice().into(),
                },
                Attribute {
                    key: b"out",
                    value: self.duration.to_string().into_bytes().into(),
                },
                Attribute {
                    key: b"mlt_service",
                    value: b"avformat".as_slice().into(),
                },
            ]),
        );

        let decl = Event::Decl(BytesDecl::new(b"1.0", Some(b"utf-8"), None));

        [decl, opener, profile, consumer]
            .into_iter()
            .chain(self.events.into_iter())
            .chain(Some(closer))
    }

    /// Write the XML events to a file.
    pub async fn write_file(self, main_tractor: &'a str) -> Result<()> {
        let out_path = self.path;
        let mut events = self.into_events(main_tractor);

        // write the events to a file
        tokio::task::block_in_place(move || {
            let file = fs::File::create(out_path.as_std_path())?;
            let file = io::BufWriter::new(file);

            // open a quick xml writer
            let mut writer = Writer::new(file);
            events.try_for_each(|event| writer.write_event(event))?;

            Ok(())
        })
    }
}

/// An entry in a playlist.
pub(crate) enum PlaylistItem<'a> {
    /// An entry referring to another entry.
    Entry { id: &'a str, start: i64, end: i64 },
    /// A blank entry.
    #[allow(dead_code)]
    Blank(i64),
}
