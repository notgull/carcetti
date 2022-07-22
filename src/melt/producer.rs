// GNU AGPL v3 License

use camino::Utf8Path;
use quick_xml::events::{attributes::Attribute, BytesEnd, BytesStart, BytesText, Event};
use std::{borrow::Cow::Borrowed, iter};

/// XML elements that are used to represent a producer.
pub(super) fn producer_xml<'a>(
    resource: &'a Utf8Path,
    id: &'a str,
) -> impl Iterator<Item = Event<'a>> {
    [
        Event::Start(
            BytesStart::borrowed_name(b"producer").with_attributes(iter::once(Attribute {
                key: b"id",
                value: id.as_bytes().into(),
            })),
        ),
        Event::Start(
            BytesStart::borrowed_name(b"property").with_attributes(iter::once(Attribute {
                key: b"name",
                value: Borrowed(b"resource"),
            })),
        ),
        Event::Text(BytesText::from_escaped_str(resource.as_str())),
        Event::End(BytesEnd::borrowed(b"property")),
        Event::End(BytesEnd::borrowed(b"producer")),
    ]
    .into_iter()
}
