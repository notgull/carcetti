// GNU AGPL v3 License

use core::iter;
use quick_xml::events::{attributes::Attribute, BytesEnd, BytesStart, Event};

/// XML for the tractor.
pub(super) fn tractor_xml<'a>(
    multitrack: impl Iterator<Item = &'a str>,
    id: &'a str,
) -> impl Iterator<Item = Event<'a>> {
    let tractor_start = Event::Start(BytesStart::borrowed_name(b"tractor").with_attributes(
        iter::once(Attribute {
            key: b"id",
            value: id.as_bytes().into(),
        }),
    ));
    let multitrack_start = Event::Start(BytesStart::borrowed_name(b"multitrack").with_attributes(
        iter::once(Attribute {
            key: b"id",
            value: id.as_bytes().into(),
        }),
    ));

    let tracks = multitrack.map(|track| {
        Event::Empty(
            BytesStart::borrowed_name(b"track").with_attributes(iter::once(Attribute {
                key: b"producer",
                value: track.as_bytes().into(),
            })),
        )
    });

    let multitrack_end = Event::End(BytesEnd::borrowed(b"multitrack"));
    let tractor_end = Event::End(BytesEnd::borrowed(b"tractor"));

    iter::once(tractor_start)
        .chain(Some(multitrack_start))
        .chain(tracks)
        .chain(Some(multitrack_end))
        .chain(Some(tractor_end))
}
