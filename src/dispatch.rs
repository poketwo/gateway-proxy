use futures_util::StreamExt;
use log::trace;
use simd_json::Mutable;
use tokio::sync::broadcast;
use twilight_gateway::{shard::Events, Event};
use twilight_model::gateway::event::GatewayEventDeserializer;

use std::sync::Arc;

use crate::{model::Ready, state::ShardStatus};

pub async fn dispatch_events(
    mut events: Events,
    shard_status: Arc<ShardStatus>,
    broadcast_tx: broadcast::Sender<String>,
) {
    while let Some(event) = events.next().await {
        match event {
            Event::ShardPayload(body) => {
                let mut payload = unsafe { String::from_utf8_unchecked(body.bytes) };
                // The event is always valid
                let deserializer = GatewayEventDeserializer::from_json(&payload).unwrap();

                // Use the raw JSON from READY to create a blank READY
                if deserializer.event_type_ref().contains(&"READY") {
                    // TODO: Ugly
                    drop(deserializer);
                    let mut ready: Ready = simd_json::from_str(&mut payload).unwrap();

                    // Clear the guilds
                    if let Some(guilds) = ready.d.get_mut("guilds") {
                        if let Some(arr) = guilds.as_array_mut() {
                            arr.clear();
                        }
                    }

                    // We don't care if it was already set
                    // since this data is timeless
                    let _res = shard_status.ready.set(ready.d);
                    shard_status.ready_set.notify_waiters();

                    continue;
                }

                // We only want to relay dispatchable events, not RESUMEs and not READY
                // because we fake a READY event
                if deserializer.op() == 0 && !deserializer.event_type_ref().contains(&"RESUMED") {
                    trace!("Sending payload to clients: {:?}", payload);
                    let _res = broadcast_tx.send(payload);
                }
            }
            Event::GuildCreate(guild_create) => {
                shard_status.guilds.insert(guild_create.0);
            }
            Event::GuildDelete(guild_delete) => {
                shard_status.guilds.remove(guild_delete.id);
            }
            Event::GuildUpdate(guild_update) => {
                shard_status.guilds.update(guild_update.0);
            }
            _ => {}
        }
    }
}