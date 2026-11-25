use lilka_emulator::{audio_output::AudioSink, qemu_live::Ipc};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
pub(super) struct AudioWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub(super) dropped: Arc<AtomicU64>,
}
impl AudioWorker {
    pub(super) fn start(ipc: Arc<Ipc>, sink: Option<AudioSink>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let done = stop.clone();
        let lost = dropped.clone();
        let join = std::thread::spawn(move || {
            let mut audio = Vec::new();
            let mut pending = Vec::new();
            let mut offset = 0;
            while !done.load(Ordering::Acquire) {
                ipc.drain_audio(&mut audio);
                if let Some(sink) = &sink {
                    if offset == pending.len() {
                        pending.clear();
                        offset = 0;
                    } else if offset > 0 {
                        pending.drain(..offset);
                        offset = 0;
                    }
                    let keep = audio.len().min(48_000usize.saturating_sub(pending.len()));
                    lost.fetch_add((audio.len() - keep) as u64, Ordering::Relaxed);
                    pending.extend_from_slice(&audio[..keep]);
                    offset += sink.push(&pending[offset..]);
                }
                std::thread::sleep(Duration::from_millis(3));
            }
        });
        Self {
            stop,
            join: Some(join),
            dropped,
        }
    }
}
impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
