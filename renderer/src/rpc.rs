use std::future::Future;
use std::io::{self, Cursor, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;

use rmpv::{Value, decode::read_value_with_max_depth, ext::from_value};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task;

use crate::export;
use crate::player::{EditorFrame, Player};
use crate::protocol::{
    EditorMetadata, PushEvent, ReplayInfo, SeekResult, Snapshot, invalid_data, pack,
};
use crate::recorder::Recording;

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Session {
    recording: Option<Recording>,
    replay: Option<Arc<Player>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Reply {
    Empty,
    Info(ReplayInfo),
    State(SeekResult),
    Frame(EditorFrame),
}

pub(crate) async fn serve(
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let mut session = Session::default();
    let result = tokio::select! {
        result = session.serve(&mut input, &mut output) => result,
        _ = shutdown => Ok(()),
    };
    let finalized = match session.recording.take() {
        Some(recording) => recording.stop().await.map(|_| ()),
        None => Ok(()),
    };
    result.and(finalized)
}

impl Session {
    async fn serve(
        &mut self,
        input: &mut (impl AsyncRead + Unpin),
        output: &mut (impl AsyncWrite + Unpin),
    ) -> io::Result<()> {
        let mut packets = Packets::default();
        while let Some(message) = packets.read(input).await? {
            let Value::Array(fields) = message else {
                return Err(invalid_data("RPC envelope must be an array"));
            };
            let mut fields = fields.into_iter();
            let kind = fields.next().and_then(|value| value.as_u64());
            let id = match (kind, fields.len()) {
                (Some(0), 3) => {
                    let id = fields
                        .next()
                        .and_then(|value| value.as_u64())
                        .and_then(|id| u32::try_from(id).ok())
                        .ok_or_else(|| invalid_data("invalid RPC request ID"))?;
                    Some(id)
                }
                (Some(2), 2) => None,
                _ => return Err(invalid_data("invalid RPC envelope")),
            };
            let result = match (fields.next(), fields.next()) {
                (Some(Value::String(method)), Some(Value::Array(params))) => {
                    match method.as_str() {
                        Some("replay_render") => self.render(params, id, output).await,
                        Some(method) => self.dispatch(method, params, id.is_some()).await,
                        None => Err(invalid_data("RPC method must be a UTF-8 string")),
                    }
                }
                _ => Err(invalid_data(
                    "RPC requires a method string and a parameter array",
                )),
            };
            if let Some(id) = id {
                let (error, reply) = match result {
                    Ok(reply) => (None, Some(reply)),
                    Err(error) => (Some(error.to_string()), None),
                };
                output.write_all(&pack(&(1, id, error, reply))?).await?;
                output.flush().await?;
            } else if let Err(error) = result {
                eprintln!("RPC notification: {error}");
            }
        }
        Ok(())
    }

    async fn dispatch(
        &mut self,
        method: &str,
        params: Vec<Value>,
        request: bool,
    ) -> io::Result<Reply> {
        match method {
            "start_recording" | "record_start" => {
                if self.recording.is_some() {
                    return Err(io::Error::other("already recording"));
                }
                if !(1..=3).contains(&params.len()) {
                    return Err(invalid_data(
                        "expected path, optional metadata, and optional snapshot",
                    ));
                }
                let mut params = params.into_iter();
                let path = PathBuf::from(required::<String>(params.next())?);
                let metadata: EditorMetadata = optional(params.next())?;
                let snapshot: Snapshot = optional(params.next())?;
                self.recording = Some(Recording::start(path, metadata, snapshot).await?);
                Ok(Reply::Empty)
            }
            "stop_recording" | "record_stop" if params.is_empty() => {
                let recording = self
                    .recording
                    .take()
                    .ok_or_else(|| io::Error::other("no recording is active"))?;
                Ok(Reply::Info(recording.stop().await?))
            }
            "push_event" | "record_event" if params.len() == 1 => {
                let recording = self
                    .recording
                    .as_ref()
                    .ok_or_else(|| io::Error::other("no recording is active"))?;
                recording
                    .push(event_parameter(params.into_iter().next())?, request)
                    .await?;
                Ok(Reply::Empty)
            }
            "open_replay" | "replay_open" if params.len() == 1 => {
                let path = PathBuf::from(required::<String>(params.into_iter().next())?);
                let replay = task::spawn_blocking(move || Player::open(&path))
                    .await
                    .map_err(io::Error::other)??;
                let info = replay.info();
                self.replay = Some(Arc::new(replay));
                Ok(Reply::Info(info))
            }
            "seek_to" | "replay_seek" if params.len() == 1 => {
                let timestamp_ns = required(params.into_iter().next())?;
                let replay = self
                    .replay
                    .clone()
                    .ok_or_else(|| io::Error::other("no replay is open"))?;
                let state = task::spawn_blocking(move || replay.seek(timestamp_ns))
                    .await
                    .map_err(io::Error::other)??;
                if method == "replay_seek" {
                    Ok(Reply::Frame(state.snapshot.into()))
                } else {
                    Ok(Reply::State(state))
                }
            }
            "stop_recording" | "record_stop" | "push_event" | "record_event" | "open_replay"
            | "replay_open" | "seek_to" | "replay_seek" => Err(invalid_data(format!(
                "invalid parameter count for {method}"
            ))),
            _ => Err(invalid_data(format!("unknown RPC method {method}"))),
        }
    }

    async fn render(
        &self,
        params: Vec<Value>,
        id: Option<u32>,
        output: &mut (impl AsyncWrite + Unpin),
    ) -> io::Result<Reply> {
        if !(2..=3).contains(&params.len()) {
            return Err(invalid_data("invalid parameter count for replay_render"));
        }
        if self.recording.is_some() {
            return Err(io::Error::other("stop recording before exporting a replay"));
        }
        let mut params = params.into_iter();
        let input = PathBuf::from(required::<String>(params.next())?);
        let destination = PathBuf::from(required::<String>(params.next())?);
        let options: export::RenderOptions = optional(params.next())?;
        let progress_id = id.filter(|_| options.progress);
        let (progress, mut updates) = watch::channel(0);
        let worker = task::spawn_blocking(move || {
            export::render(&Player::open(&input)?, &destination, &options, |percent| {
                if progress_id.is_some() {
                    progress.send_replace(percent);
                }
            })
        });
        if let Some(id) = progress_id {
            while updates.changed().await.is_ok() {
                let percent = *updates.borrow_and_update();
                output
                    .write_all(&pack(&(2, "replay_render_progress", (id, percent)))?)
                    .await?;
                output.flush().await?;
            }
        }
        worker.await.map_err(io::Error::other)??;
        Ok(Reply::Empty)
    }
}

fn event_parameter(param: Option<Value>) -> io::Result<PushEvent> {
    let Some(Value::Map(mut fields)) = param else {
        return Err(invalid_data("expected event map"));
    };
    if let Some((key, _)) = fields
        .iter_mut()
        .find(|(key, _)| key.as_str() == Some("kind"))
    {
        *key = "type".into();
    }
    if let Some((key, value)) = fields
        .iter_mut()
        .find(|(key, _)| key.as_str() == Some("windows"))
    {
        *key = "layout".into();
        *value = Value::Map(vec![(
            "windows".into(),
            std::mem::replace(value, Value::Nil),
        )]);
    }
    from_value(Value::Map(fields)).map_err(invalid_data)
}

fn required<T: DeserializeOwned>(param: Option<Value>) -> io::Result<T> {
    let param = param.ok_or_else(|| invalid_data("missing RPC parameter"))?;
    from_value(param).map_err(invalid_data)
}

fn optional<T: Default + DeserializeOwned>(param: Option<Value>) -> io::Result<T> {
    match param {
        None | Some(Value::Nil) => Ok(T::default()),
        param => required(param),
    }
}

#[derive(Default)]
struct Packets {
    bytes: Vec<u8>,
    start: usize,
}

impl Packets {
    async fn read(&mut self, input: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Value>> {
        loop {
            let rest = &self.bytes[self.start..];
            if !rest.is_empty() {
                let mut cursor = Cursor::new(rest);
                match read_value_with_max_depth(&mut cursor, 64) {
                    Ok(message) => {
                        self.start += cursor.position() as usize;
                        return Ok(Some(message));
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => {}
                    Err(error) => return Err(invalid_data(error)),
                }
            }
            let remaining = MAX_REQUEST_BYTES - rest.len();
            if remaining == 0 {
                return Err(invalid_data("RPC message exceeds size limit"));
            }
            if self.start != 0 {
                self.bytes.copy_within(self.start.., 0);
                self.bytes.truncate(self.bytes.len() - self.start);
                self.start = 0;
            }
            let mut buffer = [0; 8192];
            let limit = remaining.min(buffer.len());
            let read = input.read(&mut buffer[..limit]).await?;
            if read == 0 {
                return if self.bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "incomplete RPC message",
                    ))
                };
            }
            self.bytes.extend_from_slice(&buffer[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::field;

    use super::*;

    #[tokio::test]
    async fn fragmented_requests_and_coalesced_notifications_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.nvimreplay");
        let (client, server) = tokio::io::duplex(1024);
        let (input, output) = tokio::io::split(server);
        let session = tokio::spawn(serve(input, output, std::future::pending()));
        let (mut input, mut output) = tokio::io::split(client);
        let mut packets = Packets::default();
        let start = pack(&(0, 7, "start_recording", [path.to_str().unwrap()])).unwrap();
        output.write_all(&start[..3]).await.unwrap();
        tokio::task::yield_now().await;
        output.write_all(&start[3..]).await.unwrap();
        let started = packets.read(&mut input).await.unwrap().unwrap();
        assert_eq!(
            started,
            Value::Array(vec![1.into(), 7.into(), Value::Nil, Value::Nil])
        );

        let buf = Value::Map(vec![
            ("kind".into(), "buf_snapshot".into()),
            ("buf".into(), 3.into()),
            ("timestamp_ns".into(), 0.into()),
            ("lines".into(), Value::Array(vec!["before".into()])),
        ]);
        let enter = Value::Map(vec![
            ("type".into(), "buf_enter".into()),
            ("buffer_id".into(), 3.into()),
            ("window_id".into(), 2.into()),
            ("timestamp_ns".into(), 0.into()),
        ]);
        let layout = Value::Map(vec![
            ("kind".into(), "win_layout".into()),
            ("timestamp_ns".into(), 0.into()),
            (
                "windows".into(),
                Value::Array(vec![Value::Map(vec![
                    ("win".into(), 2.into()),
                    ("topline".into(), 4.into()),
                ])]),
            ),
        ]);
        let messages = [
            pack(&(2, "record_event", [buf])).unwrap(),
            pack(&(2, "push_event", [enter])).unwrap(),
            pack(&(2, "record_event", [layout])).unwrap(),
            pack(&(0, 8, "stop_recording", [(); 0])).unwrap(),
        ]
        .concat();
        output.write_all(&messages).await.unwrap();
        let stop = packets.read(&mut input).await.unwrap().unwrap();
        assert_eq!(stop.as_array().unwrap()[1..3], [8.into(), Value::Nil]);
        assert_eq!(
            field(&stop.as_array().unwrap()[3], "keyframes"),
            Some(&Value::Array(vec![0.into()]))
        );

        output
            .write_all(&pack(&(0, 9, "open_replay", [path.to_str().unwrap()])).unwrap())
            .await
            .unwrap();
        let open = packets.read(&mut input).await.unwrap().unwrap();
        assert_eq!(open.as_array().unwrap()[1..3], [9.into(), Value::Nil]);

        output
            .write_all(&pack(&(0, 10, "seek_to", [0])).unwrap())
            .await
            .unwrap();
        let seek = packets.read(&mut input).await.unwrap().unwrap();
        let [_, id, err, result] = seek.as_array().unwrap().as_slice() else {
            panic!("invalid response")
        };
        assert_eq!((id.as_u64(), err), (Some(10), &Value::Nil));
        let state: SeekResult = from_value(result.clone()).unwrap();
        assert_eq!(state.snapshot.buffers[&3].lines, ["before"]);
        assert_eq!(state.snapshot.active_window, Some(2));

        output
            .write_all(&pack(&(0, 11, "replay_seek", [0])).unwrap())
            .await
            .unwrap();
        let seek = packets.read(&mut input).await.unwrap().unwrap();
        let result = &seek.as_array().unwrap()[3];
        assert_eq!(
            field(result, "lines"),
            Some(&Value::Array(vec!["before".into()]))
        );
        assert_eq!(field(result, "topline"), Some(&4.into()));

        output.shutdown().await.unwrap();
        session.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn truncated_rpc_packet_is_distinct_from_clean_eof() {
        let mut packets = Packets::default();
        let mut empty = &[][..];
        assert!(packets.read(&mut empty).await.unwrap().is_none());
        let mut truncated = &[0x93, 0x02][..];
        assert_eq!(
            packets.read(&mut truncated).await.unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }
}
