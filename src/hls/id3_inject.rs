pub struct TrackMetadata<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub artwork_url: &'a str,
}

pub fn build_id3v2(meta: &TrackMetadata) -> Vec<u8> {
    let mut frames = Vec::new();

    if !meta.title.is_empty() {
        frames.extend_from_slice(&build_text_frame(b"TIT2", meta.title));
    }
    if !meta.artist.is_empty() {
        frames.extend_from_slice(&build_text_frame(b"TPE1", meta.artist));
    }
    if !meta.artwork_url.is_empty() {
        frames.extend_from_slice(&build_wxxx_frame(meta.artwork_url));
    }

    if frames.is_empty() {
        return Vec::new();
    }

    let mut tag = Vec::with_capacity(10 + frames.len());
    tag.extend_from_slice(b"ID3");
    tag.push(0x04); // v2.4
    tag.push(0x00);
    tag.push(0x00);
    tag.extend_from_slice(&syncsafe_u32(frames.len() as u32));
    tag.extend_from_slice(&frames);
    tag
}

fn build_text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let payload_len = 1 + text_bytes.len();

    let mut frame = Vec::with_capacity(10 + payload_len);
    frame.extend_from_slice(id);
    frame.extend_from_slice(&syncsafe_u32(payload_len as u32));
    frame.extend_from_slice(&[0x00, 0x00]);
    frame.push(0x03); // UTF-8
    frame.extend_from_slice(text_bytes);
    frame
}

fn build_wxxx_frame(url: &str) -> Vec<u8> {
    let description = b"artworkURL_640x";
    let url_bytes = url.as_bytes();
    let payload_len = 1 + description.len() + 1 + url_bytes.len();

    let mut frame = Vec::with_capacity(10 + payload_len);
    frame.extend_from_slice(b"WXXX");
    frame.extend_from_slice(&syncsafe_u32(payload_len as u32));
    frame.extend_from_slice(&[0x00, 0x00]);
    frame.push(0x03); // UTF-8
    frame.extend_from_slice(description);
    frame.push(0x00);
    frame.extend_from_slice(url_bytes);
    frame
}

fn syncsafe_u32(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id3v2_structure() {
        let meta = TrackMetadata {
            title: "Test Song",
            artist: "Test Artist",
            artwork_url: "http://example.com/art.jpg",
        };
        let tag = build_id3v2(&meta);
        assert_eq!(&tag[0..3], b"ID3");
        assert_eq!(tag[3], 0x04); // v2.4
        assert!(tag.windows(4).any(|w| w == b"TIT2"));
        assert!(tag.windows(4).any(|w| w == b"TPE1"));
        assert!(tag.windows(4).any(|w| w == b"WXXX"));
    }

    #[test]
    fn test_empty_metadata() {
        let meta = TrackMetadata {
            title: "",
            artist: "",
            artwork_url: "",
        };
        assert!(build_id3v2(&meta).is_empty());
    }
}
