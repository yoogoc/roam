use rodio::Source;
use std::{
    io::Cursor,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

enum Command {
    Toggle,
    Seek(Duration),
    Stop,
}
#[derive(Clone, Default)]
pub(super) struct Snapshot {
    pub position: Duration,
    pub duration: Option<Duration>,
    pub paused: bool,
    pub metadata: String,
    pub error: Option<String>,
}
pub(super) struct AudioPlayer {
    commands: mpsc::Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
}
impl AudioPlayer {
    pub fn new(bytes: Vec<u8>) -> Result<Self, String> {
        let (commands, receiver) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let status = snapshot.clone();
        std::thread::Builder::new()
            .name("roam-audio".into())
            .spawn(move || {
                let start = || -> Result<_, String> {
                    let bytes: Arc<[u8]> = bytes.into();
                    let decoder = decoder(bytes.clone())?;
                    let mut device = rodio::DeviceSinkBuilder::open_default_sink()
                        .map_err(|e| format!("无法打开音频设备：{e}"))?;
                    device.log_on_drop(false);
                    let player = rodio::Player::connect_new(device.mixer());
                    *status.lock().unwrap() = Snapshot {
                        duration: decoder.total_duration(),
                        paused: true,
                        metadata: format!(
                            "{} Hz · {} 声道",
                            decoder.sample_rate(),
                            decoder.channels()
                        ),
                        ..Default::default()
                    };
                    player.pause();
                    player.append(decoder);
                    Ok((device, player, bytes))
                };
                let (_device, player, bytes) = match start() {
                    Ok(pair) => {
                        let _ = ready.send(Ok(()));
                        pair
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(Command::Toggle) => {
                            if player.empty() {
                                match decoder(bytes.clone()) {
                                    Ok(source) => {
                                        player.append(source);
                                        player.play();
                                    }
                                    Err(error) => status.lock().unwrap().error = Some(error),
                                }
                            } else if player.is_paused() {
                                player.play();
                            } else {
                                player.pause();
                            }
                        }
                        Ok(Command::Seek(position)) => {
                            if let Err(error) = player.try_seek(position) {
                                status.lock().unwrap().error =
                                    Some(format!("无法定位音频：{error}"));
                            }
                        }
                        Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            player.stop();
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let mut status = status.lock().unwrap();
                    status.position = player.get_pos();
                    status.paused = player.is_paused() || player.empty();
                }
            })
            .map_err(|e| e.to_string())?;
        initialized
            .recv_timeout(Duration::from_secs(20))
            .map_err(|e| e.to_string())??;
        Ok(Self { commands, snapshot })
    }
    pub fn toggle(&self) {
        let _ = self.commands.send(Command::Toggle);
    }
    pub fn seek(&self, delta: i64) {
        let snapshot = self.snapshot();
        let seconds = (snapshot.position.as_secs_f64() + delta as f64).max(0.);
        let position =
            Duration::from_secs_f64(seconds).min(snapshot.duration.unwrap_or(Duration::MAX));
        let _ = self.commands.send(Command::Seek(position));
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }
}
fn decoder(bytes: Arc<[u8]>) -> Result<rodio::Decoder<Cursor<Arc<[u8]>>>, String> {
    let length = bytes.len() as u64;
    rodio::Decoder::builder()
        .with_data(Cursor::new(bytes))
        .with_byte_len(length)
        .with_seekable(true)
        .build()
        .map_err(|e| e.to_string())
}
impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}
