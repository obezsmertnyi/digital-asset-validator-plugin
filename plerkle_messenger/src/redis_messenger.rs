use crate::{error::MessengerError, Messenger, MessengerConfig, MessengerType, RecvData};
use async_trait::async_trait;
use log::*;
use redis::{
    aio::AsyncStream,
    cmd,
    streams::{StreamId, StreamKey, StreamMaxlen, StreamReadOptions, StreamReadReply},
    AsyncCommands, RedisResult, Value,
};

use redis::streams::StreamRangeReply;
use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    pin::Pin,
};

// Redis stream values.
pub const GROUP_NAME: &str = "plerkle";
pub const DATA_KEY: &str = "data";

#[derive(Default)]
pub struct RedisMessenger {
    connection: Option<redis::aio::Connection<Pin<Box<dyn AsyncStream + Send + Sync>>>>,
    streams: HashMap<&'static str, RedisMessengerStream>,
    stream_read_reply: StreamReadReply,
    consumer_id: String,
}

pub struct RedisMessengerStream {
    buffer_size: Option<StreamMaxlen>,
}

const REDIS_CON_STR: &str = "redis_connection_str";

impl RedisMessenger {
    async fn xautoclaim(&mut self, stream_key: &str) -> Result<StreamRangeReply, MessengerError> {
        let mut id = "0-0".to_owned();
        // We need to call `XAUTOCLAIM` repeatedly because it will (according to the docs)
        // only look at up to 10 * `count` PEL entries each time, and `id` is used to
        // know where we left off to continue from next call.
        loop {
            // The `redis` crate doesn't appear to support this command so we have
            // to call it via the lower level primitives it provides.
            let mut xauto = cmd("XAUTOCLAIM");
            xauto
                .arg(stream_key)
                .arg(GROUP_NAME)
                .arg(self.consumer_id.as_str())
                // We only reclaim items that have been idle for at least 2 sec.
                .arg(2000)
                .arg(id.as_str())
                // For now, we're only looking for one message.
                .arg("COUNT")
                .arg(1);

            // Before Redis 7 (we're using 6.2.x presently), `XAUTOCLAIM` returns an array of
            // two items: an id to be used for the next call to continue scanning the PEL,
            // and a list of successfully claimed messages in the same format as `XRANGE`.
            let result: (String, StreamRangeReply) = xauto
                .query_async(self.connection.as_mut().unwrap())
                .await
                .map_err(|e| MessengerError::AutoclaimError { msg: e.to_string() })?;

            id = result.0;
            let range_reply = result.1;

            // An id of "0-0" means all the PEL has been searched so we need to return anyway,
            // even if the reply is empty. We also want to immediately return if we have
            // a non-empty reply.
            if id == "0-0" || !range_reply.ids.is_empty() {
                return Ok(range_reply);
            }
        }
    }
}

#[async_trait]
impl Messenger for RedisMessenger {
    //pub async fn new(stream_key: &'static str) -> Result<Self> {
    async fn new(config: MessengerConfig) -> Result<Self, MessengerError> {
        let uri = config
            .get(&*REDIS_CON_STR)
            .and_then(|u| u.clone().into_string())
            .ok_or(MessengerError::ConfigurationError {
                msg: format!("Connection String Missing: {}", REDIS_CON_STR),
            })?;
        // Setup Redis client.
        let client = redis::Client::open(uri).unwrap();

        // Get connection.
        let connection = client.get_tokio_connection().await.map_err(|e| {
            error!("{}", e.to_string());
            MessengerError::ConnectionError { msg: e.to_string() }
        })?;

        let consumer_id = config
            .get("consumer_id")
            .and_then(|id| id.clone().into_string())
            // Using the previous default name when the configuration does not
            // specify any particular consumer_id.
            .unwrap_or(String::from("ingester"));

        Ok(Self {
            connection: Some(connection),
            streams: HashMap::<&'static str, RedisMessengerStream>::default(),
            stream_read_reply: StreamReadReply::default(),
            consumer_id,
        })
    }

    fn messenger_type(&self) -> MessengerType {
        MessengerType::Redis
    }

    async fn add_stream(&mut self, stream_key: &'static str) -> Result<(), MessengerError> {
        // Add to streams hashmap.
        let _result = self
            .streams
            .insert(stream_key, RedisMessengerStream { buffer_size: None });

        // Add stream to Redis.
        let result: RedisResult<()> = self
            .connection
            .as_mut()
            .unwrap()
            .xgroup_create_mkstream(stream_key, GROUP_NAME, "$")
            .await;

        if let Err(e) = result {
            info!("Group already exists: {:?}", e)
        }
        Ok(())
    }

    async fn set_buffer_size(&mut self, stream_key: &'static str, max_buffer_size: usize) {
        // Set max length for the stream.
        if let Some(stream) = self.streams.get_mut(stream_key) {
            stream.buffer_size = Some(StreamMaxlen::Approx(max_buffer_size));
        } else {
            error!("Stream key {stream_key} not configured");
        }
    }

    async fn send(&mut self, stream_key: &'static str, bytes: &[u8]) -> Result<(), MessengerError> {
        // Check if stream is configured.
        let stream = if let Some(stream) = self.streams.get(stream_key) {
            stream
        } else {
            error!("Cannot send data for stream key {stream_key}, it is not configured");
            return Ok(());
        };

        // Get max length for the stream.
        let maxlen = if let Some(maxlen) = stream.buffer_size {
            maxlen
        } else {
            error!("Cannot send data for stream key {stream_key}, buffer size not set.");
            return Ok(());
        };

        // Put serialized data into Redis.
        let result: RedisResult<()> = self
            .connection
            .as_mut()
            .unwrap()
            .xadd_maxlen(stream_key, maxlen, "*", &[(DATA_KEY, &bytes)])
            .await;

        if let Err(e) = result {
            error!("Redis send error: {e}");
            return Err(MessengerError::SendError { msg: e.to_string() });
        } else {
            info!("Data Sent to {}", stream_key);
        }

        Ok(())
    }

    async fn recv(&mut self, stream_key: &'static str) -> Result<Vec<RecvData>, MessengerError> {
        let xauto_reply = self.xautoclaim(stream_key).await?;

        if !xauto_reply.ids.is_empty() {
            // We construct a `StreamReadReply` to match the expected type we store
            // in `self`. This is possible because the two types we're working with
            // have a compatible inner structure.
            self.stream_read_reply = StreamReadReply {
                keys: vec![StreamKey {
                    key: stream_key.to_owned(),
                    ids: xauto_reply.ids,
                }],
            };
        } else {
            let opts = StreamReadOptions::default()
                // Wait for up to 2 sec for a message. We're no longer blocking indefinitely
                // here to avoid situations where we might be blocked on `XREAD` while pending
                // messages accumulate that can be claimed.
                .block(2000)
                .count(1) // Get one item.
                .group(GROUP_NAME, self.consumer_id.as_str());

            // Read on stream key and save the reply. Log but do not return errors.
            self.stream_read_reply = match self
                .connection
                .as_mut()
                .unwrap()
                .xread_options(&[stream_key], &[">"], &opts)
                .await
            {
                Ok(reply) => reply,
                Err(e) => {
                    error!("Redis receive error: {e}");
                    return Err(MessengerError::ReceiveError { msg: e.to_string() });
                }
            };
        }

        // Data vec that will be returned with parsed data from stream read reply. Since
        // we're only waiting for up to 2 seconds for `XREAD` to return, we may end up
        // returning an empty vec, and the caller will have to call `recv` again.
        let mut data_vec = Vec::new();

        // Parse data in stream read reply and store in Vec to return to caller.
        for StreamKey { key, ids } in self.stream_read_reply.keys.iter() {
            if key == stream_key {
                for StreamId { id, map } in ids {
                    // Get data from map.
                    let data = if let Some(data) = map.get(DATA_KEY) {
                        data
                    } else {
                        println!("No Data was stored in Redis for ID {id}");
                        continue;
                    };
                    let bytes = match data {
                        Value::Data(bytes) => bytes,
                        _ => {
                            println!("Redis data for ID {id} in wrong format");
                            continue;
                        }
                    };

                    data_vec.push(RecvData::new(id.clone(), bytes));
                }
            }
        }

        Ok(data_vec)
    }

    async fn ack_msg(
        &mut self,
        stream_key: &'static str,
        ids: &[String],
    ) -> Result<(), MessengerError> {
        if ids.is_empty() {
            return Ok(());
        }

        self.connection
            .as_mut()
            .unwrap()
            .xack(stream_key, GROUP_NAME, ids)
            .await
            .map_err(|e| MessengerError::AckError { msg: e.to_string() })
    }
}

impl Debug for RedisMessenger {
    fn fmt(&self, _f: &mut Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}
