// GNU AGPL v3 License

use super::PlaylistItem;
use quick_xml::events::{attributes::Attribute, BytesEnd, BytesStart, Event};
use std::{borrow::Cow::Borrowed, iter};

/// XML events that are used to represent a playlist.
pub(super) fn playlist_xml<'a>(
    entries: impl Iterator<Item = PlaylistItem<'a>>,
    id: &'a str,
) -> impl Iterator<Item = Event<'a>> {
    let start = Event::Start(
        BytesStart::borrowed_name(b"playlist").with_attributes(iter::once(Attribute {
            key: b"id",
            value: id.as_bytes().into(),
        })),
    );

    let items = entries.map(|event| {
        Event::Empty(match event {
            PlaylistItem::Entry { id, start, end } => BytesStart::borrowed_name(b"entry")
                .with_attributes([
                    Attribute {
                        key: b"producer",
                        value: id.as_bytes().into(),
                    },
                    Attribute {
                        key: b"in",
                        value: start.to_string().into_bytes().into(),
                    },
                    Attribute {
                        key: b"out",
                        value: end.to_string().into_bytes().into(),
                    },
                ]),
            PlaylistItem::Blank(length) => {
                BytesStart::borrowed_name(b"blank").with_attributes(iter::once(Attribute {
                    key: b"length",
                    value: length.to_string().into_bytes().into(),
                }))
            }
        })
    });

    let end = Event::End(BytesEnd::borrowed(b"playlist"));

    iter::once(start).chain(items).chain(iter::once(end))
}
