use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ipc::framing::{read_message, write_message};
use crate::ipc::proto::{ClientMessage, ServerMessage};

pub struct IpcClient {
    writer: Arc<Mutex<UnixStream>>,
    receiver: mpsc::Receiver<ServerMessage>,
    build_id: u64,
}

impl IpcClient {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));

        // Read Welcome synchronously before spawning the reader thread
        let build_id = match read_message::<ServerMessage>(&mut reader) {
            Ok(ServerMessage::Welcome { build_id, .. }) => build_id,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected Welcome",
                ))
            }
            Err(e) => return Err(e),
        };

        // Clear timeout for async reader
        reader
            .set_read_timeout(None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader;
            loop {
                match read_message::<ServerMessage>(&mut reader) {
                    Ok(msg) => {
                        let _ = sender.send(msg);
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            writer,
            receiver,
            build_id,
        })
    }

    pub fn build_id(&self) -> u64 {
        self.build_id
    }

    pub fn send(&self, msg: &ClientMessage) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = write_message(&mut *writer, msg);
        }
    }

    pub fn try_recv(&self) -> Option<ServerMessage> {
        self.receiver.try_recv().ok()
    }
}
