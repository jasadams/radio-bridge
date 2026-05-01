use bytes::Bytes;
use std::collections::VecDeque;

pub struct Segment {
    pub seq: u64,
    pub data: Bytes,
    pub duration: f64,
}

pub struct SegmentStore {
    segments: VecDeque<Segment>,
    max_segments: usize,
    next_seq: u64,
}

impl SegmentStore {
    pub fn new(max_segments: usize) -> Self {
        Self {
            segments: VecDeque::with_capacity(max_segments + 1),
            max_segments,
            next_seq: 0,
        }
    }

    pub fn add(&mut self, data: Bytes, duration: f64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.segments.push_back(Segment {
            seq,
            data,
            duration,
        });
        while self.segments.len() > self.max_segments {
            self.segments.pop_front();
        }
        seq
    }

    pub fn get(&self, seq: u64) -> Option<&Bytes> {
        self.segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| &s.data)
    }

    pub fn generate_playlist(&self, base_url: &str, channel: &str) -> String {
        if self.segments.len() < 3 {
            return String::new();
        }

        let target_duration = self
            .segments
            .iter()
            .map(|s| s.duration.ceil() as u64)
            .max()
            .unwrap_or(10);

        let first_seq = self.segments.front().map(|s| s.seq).unwrap_or(0);

        let mut m3u8 = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:{target_duration}\n\
             #EXT-X-MEDIA-SEQUENCE:{first_seq}\n"
        );

        for seg in &self.segments {
            m3u8.push_str(&format!(
                "#EXTINF:{:.3},\n{base_url}/hls/{channel}/seg/{}.ts\n",
                seg.duration, seg.seq
            ));
        }

        m3u8
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn count(&self) -> usize {
        self.segments.len()
    }

    pub fn first_seq(&self) -> u64 {
        self.segments.front().map(|s| s.seq).unwrap_or(0)
    }

    pub fn last_seq(&self) -> u64 {
        self.segments.back().map(|s| s.seq).unwrap_or(0)
    }
}
