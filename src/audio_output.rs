use anyhow::{Result, anyhow};
use maudio::{
    audio::sample_rate::SampleRate,
    device::{
        Device,
        device_builder::{DeviceBuilder, DeviceBuilderOps},
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

const PLAYBACK_FRAMES: usize = crate::hardware::AUDIO_RATE as usize / 10;
const PREBUFFER_FRAMES: usize = crate::hardware::AUDIO_RATE as usize / 20;

struct PcmQueue {
    samples: Box<[AtomicU32]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    clear: AtomicBool,
    clear_to: AtomicUsize,
}
impl PcmQueue {
    fn new() -> Self {
        Self {
            samples: (0..PLAYBACK_FRAMES).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            clear: AtomicBool::new(false),
            clear_to: AtomicUsize::new(0),
        }
    }
    fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(tail)
            .min(PLAYBACK_FRAMES)
    }
    fn push_slice(&self, frames: &[[i16; 2]]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let available = PLAYBACK_FRAMES.saturating_sub(head.wrapping_sub(tail));
        let count = frames.len().min(available);
        let start = head % PLAYBACK_FRAMES;
        let first = count.min(PLAYBACK_FRAMES - start);
        let slots = self.samples[start..start + first]
            .iter()
            .chain(self.samples[..count - first].iter());
        for (slot, frame) in slots.zip(&frames[..count]) {
            let bits = u32::from(frame[0] as u16) | (u32::from(frame[1] as u16) << 16);
            slot.store(bits, Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(count), Ordering::Release);
        count
    }
    #[cfg(test)]
    fn push(&self, sample: [i16; 2]) -> bool {
        self.push_slice(&[sample]) == 1
    }
    fn read_into(&self, out: &mut [i16]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let count = head.wrapping_sub(tail).min(out.len() / 2);
        let start = tail % PLAYBACK_FRAMES;
        let first = count.min(PLAYBACK_FRAMES - start);
        let slots = self.samples[start..start + first]
            .iter()
            .chain(self.samples[..count - first].iter());
        for (slot, frame) in slots.zip(out.chunks_exact_mut(2)) {
            let bits = slot.load(Ordering::Relaxed);
            frame[0] = bits as u16 as i16;
            frame[1] = (bits >> 16) as u16 as i16;
        }
        self.tail.store(tail.wrapping_add(count), Ordering::Release);
        count
    }
    #[cfg(test)]
    fn pop(&self) -> Option<[i16; 2]> {
        let mut sample = [0; 2];
        (self.read_into(&mut sample) == 1).then_some(sample)
    }
    fn request_clear(&self) {
        self.clear_to
            .store(self.head.load(Ordering::Acquire), Ordering::Relaxed);
        self.clear.store(true, Ordering::Release);
    }
    fn clear_on_consumer(&self) -> bool {
        if !self.clear.swap(false, Ordering::AcqRel) {
            return false;
        }
        let cutoff = self.clear_to.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed);
        if cutoff.wrapping_sub(tail) <= PLAYBACK_FRAMES {
            self.tail.store(cutoff, Ordering::Release);
        }
        true
    }
}
#[derive(Default)]
struct Playback {
    primed: bool,
    started: bool,
    last: [i16; 2],
}
impl Playback {
    fn render(&mut self, queue: &PcmQueue, out: &mut [i16]) -> (usize, usize) {
        if queue.clear_on_consumer() {
            *self = Self::default();
        }
        if !self.primed && queue.len() >= PREBUFFER_FRAMES {
            self.primed = true;
            self.started = true;
        }
        let count = if self.primed { queue.read_into(out) } else { 0 };
        if count > 0 {
            self.last.copy_from_slice(&out[count * 2 - 2..count * 2]);
        }
        let missing = out.len() / 2 - count;
        if missing > 0 {
            self.primed = false;
            for frame in out[count * 2..].chunks_exact_mut(2) {
                self.last = self.last.map(|sample| (i32::from(sample) * 31 / 32) as i16);
                frame.copy_from_slice(&self.last);
            }
        }
        (count, if self.started { missing } else { 0 })
    }
}
#[derive(Clone)]
pub struct AudioSink {
    queue: Arc<PcmQueue>,
}
impl AudioSink {
    pub fn push(&self, frames: &[[i16; 2]]) -> usize {
        self.queue.push_slice(frames)
    }
}
pub struct AudioOutput {
    _device: Device<i16>,
    queue: Arc<PcmQueue>,
    pub played: Arc<AtomicU64>,
    pub underruns: Arc<AtomicU64>,
}
impl AudioOutput {
    pub fn new() -> Result<Self> {
        Self::with_context(None)
    }
    pub fn null() -> Result<Self> {
        let context = maudio::context::ContextBuilder::new()
            .preferred_backends([maudio::backend::Backend::Null])
            .build()
            .map_err(|e| anyhow!("Null audio: {e}"))?;
        Self::with_context(Some(&context))
    }
    fn with_context(context: Option<&maudio::context::Context>) -> Result<Self> {
        let queue = Arc::new(PcmQueue::new());
        let played = Arc::new(AtomicU64::new(0));
        let underruns = Arc::new(AtomicU64::new(0));
        let callback_queue = queue.clone();
        let played_frames = played.clone();
        let missed_frames = underruns.clone();
        let mut playback = Playback::default();
        let mut builder = DeviceBuilder::playback().i16();
        builder
            .playback_channels(2)
            .sample_rate(SampleRate::Sr48000);
        if let Some(context) = context {
            builder.context(context);
        }
        let mut device = builder
            .with_callback(move |_, out| {
                let (count, missing) = playback.render(&callback_queue, out);
                played_frames.fetch_add(count as u64, Ordering::Relaxed);
                missed_frames.fetch_add(missing as u64, Ordering::Relaxed);
            })
            .map_err(|e| anyhow!("Audio device: {e}"))?;
        device
            .device_start()
            .map_err(|e| anyhow!("Audio start: {e}"))?;
        Ok(Self {
            _device: device,
            queue,
            played,
            underruns,
        })
    }
    pub fn sink(&self) -> AudioSink {
        AudioSink {
            queue: self.queue.clone(),
        }
    }
    pub fn push(&self, frames: Vec<[i16; 2]>) -> Result<()> {
        anyhow::ensure!(
            frames.len() <= PLAYBACK_FRAMES.saturating_sub(self.queue.len()),
            "Audio queue full; use AudioSink to retain pending PCM frames"
        );
        let written = self.sink().push(&frames);
        anyhow::ensure!(
            written == frames.len(),
            "AudioSink requires a single producer"
        );
        Ok(())
    }
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }
    pub fn clear(&self) {
        self.queue.request_clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_preserves_order_and_defers_clear_to_consumer() {
        let q = PcmQueue::new();
        assert!(q.push([123, -456]));
        assert_eq!(q.pop(), Some([123, -456]));
        assert!(q.push([1, 2]));
        q.request_clear();
        assert!(q.clear_on_consumer());
        assert!(q.pop().is_none());
    }
    #[test]
    fn clear_preserves_frames_published_after_request() {
        let q = PcmQueue::new();
        q.push([1, 2]);
        q.request_clear();
        q.push([3, 4]);
        assert!(q.clear_on_consumer());
        assert_eq!(q.pop(), Some([3, 4]));
        assert_eq!(q.pop(), None);
        q.push([5, 6]);
        q.request_clear();
        assert_eq!(q.pop(), Some([5, 6]));
        q.push([7, 8]);
        assert!(q.clear_on_consumer());
        assert_eq!(q.pop(), Some([7, 8]));
    }
    #[test]
    fn producer_retains_frames_when_playback_queue_is_full() {
        let queue = Arc::new(PcmQueue::new());
        let sink = AudioSink {
            queue: queue.clone(),
        };
        let frames = vec![[7, -7]; PLAYBACK_FRAMES + 4];
        let written = sink.push(&frames);
        assert_eq!(written, PLAYBACK_FRAMES);
        for _ in 0..4 {
            assert_eq!(queue.pop(), Some([7, -7]));
        }
        assert_eq!(sink.push(&frames[written..]), 4);
    }
    #[test]
    fn batched_frames_keep_order_across_wrap() {
        let queue = Arc::new(PcmQueue::new());
        let sink = AudioSink {
            queue: queue.clone(),
        };
        let frames: Vec<_> = (0..PLAYBACK_FRAMES)
            .map(|i| [i as i16, -(i as i16)])
            .collect();
        assert_eq!(sink.push(&frames), frames.len());
        for frame in &frames[..100] {
            assert_eq!(queue.pop(), Some(*frame));
        }
        assert_eq!(sink.push(&frames[..100]), 100);
        for frame in &frames[100..] {
            assert_eq!(queue.pop(), Some(*frame));
        }
        for frame in &frames[..100] {
            assert_eq!(queue.pop(), Some(*frame));
        }
        assert_eq!(queue.pop(), None);
    }
    #[test]
    fn producer_and_callback_keep_every_frame_across_many_wraps() {
        let queue = Arc::new(PcmQueue::new());
        let producer = queue.clone();
        let count = 500_000usize;
        let writer = std::thread::spawn(move || {
            let mut offset = 0;
            while offset < count {
                let end = (offset + 317).min(count);
                let frames: Vec<_> = (offset..end).map(|i| [i as i16, !(i as i16)]).collect();
                let written = producer.push_slice(&frames);
                offset += written;
                if written == 0 {
                    std::thread::yield_now();
                }
            }
        });
        let mut out = [0i16; 2 * 251];
        let mut offset = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while offset < count {
            assert!(std::time::Instant::now() < deadline, "PCM transfer stalled");
            let read = queue.read_into(&mut out);
            for (i, frame) in out[..read * 2].chunks_exact(2).enumerate() {
                let expected = (offset + i) as i16;
                assert_eq!(frame, &[expected, !expected]);
            }
            offset += read;
            if read == 0 {
                std::thread::yield_now();
            }
        }
        writer.join().unwrap();
        assert_eq!(queue.len(), 0);
    }
    #[test]
    fn playback_rebuffers_without_losing_or_changing_pcm() {
        let q = PcmQueue::new();
        let mut playback = Playback::default();
        let mut out = vec![9; 512 * 2];
        assert_eq!(playback.render(&q, &mut out), (0, 0));
        assert!(out.iter().all(|&s| s == 0));
        let frames: Vec<_> = (0..PREBUFFER_FRAMES)
            .map(|i| [i as i16, -(i as i16)])
            .collect();
        q.push_slice(&frames);
        let mut received = Vec::new();
        while q.len() > 0 {
            let (n, _) = playback.render(&q, &mut out);
            received.extend(out[..n * 2].chunks_exact(2).map(|s| [s[0], s[1]]));
        }
        assert_eq!(received, frames);
        assert!(!playback.primed);
        q.push_slice(&frames[..100]);
        assert_eq!(playback.render(&q, &mut out), (0, 512));
        assert_eq!(q.len(), 100);
        q.push_slice(&frames[100..]);
        assert_eq!(playback.render(&q, &mut out), (512, 0));
        assert_eq!(&out[..4], &[0, 0, 1, -1]);
        q.request_clear();
        assert_eq!(playback.render(&q, &mut out), (0, 0));
        assert!(out.iter().all(|&s| s == 0));
    }
    #[test]
    fn actual_device_callback_consumes_pcm_and_queue_is_bounded() -> Result<()> {
        let context = maudio::context::ContextBuilder::new()
            .preferred_backends([maudio::backend::Backend::Null])
            .build()
            .map_err(|e| anyhow!("Null audio: {e}"))?;
        let audio = AudioOutput::with_context(Some(&context))?;
        assert!(audio.push(vec![[123, -123]; PLAYBACK_FRAMES + 1]).is_err());
        assert_eq!(audio.queued_frames(), 0);
        audio.push(vec![[123, -123]; PLAYBACK_FRAMES])?;
        assert!(audio.queued_frames() <= PLAYBACK_FRAMES);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while audio.played.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            audio.played.load(Ordering::Relaxed) > 0,
            "native callback did not consume PCM"
        );
        audio.clear();
        Ok(())
    }
}
