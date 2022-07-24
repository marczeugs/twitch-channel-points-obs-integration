#![feature(let_chains)]
#![feature(async_closure)]

use std::{env, fs};
use std::sync::Arc;
use std::time::Duration;
use anyhow::Context;
use twitch_api2::twitch_oauth2;
use futures_util::{SinkExt, StreamExt};
use twitch_api2::pubsub::Topic;

#[derive(serde::Deserialize)]
struct Config {
    enable_backtrace: Option<bool>,
    twitch_token: String,
    twitch_channel: String,
    obs_websocket_ip: String,
    obs_websocket_port: u16,
    obs_websocket_password: Option<String>,
    redemption_mapping: Vec<RedemptionMapping>
}

#[derive(serde::Deserialize)]
struct RedemptionMapping {
    name: String,
    action: RedemptionMappingAction
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RedemptionMappingAction {
    ToggleFilterVisibility {
        source: String,
        filter: String
    }
}

#[derive(serde::Serialize)]
struct TwitchPubSubRequest {
    r#type: String
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();


    let config = serde_json::from_str::<Config>(&fs::read_to_string("config.json").with_context(|| "Could not find config.json")?)?;


    if let Some(enable_backtrace) = config.enable_backtrace && enable_backtrace {
        env::set_var("RUST_BACKTRACE", "1");
    }


    let obs_client = obws::Client::connect(config.obs_websocket_ip, config.obs_websocket_port).await?;
    obs_client.login(config.obs_websocket_password).await?;


    let twitch_client: twitch_api2::HelixClient<reqwest::Client> = twitch_api2::HelixClient::default();

    let twitch_user_token = {
        let access_token = twitch_oauth2::AccessToken::new(config.twitch_token);
        twitch_oauth2::UserToken::from_existing(&twitch_client, access_token, None, None).await?
    };

    let twitch_channel_id = twitch_client.get_user_from_login(config.twitch_channel.to_string(), &twitch_user_token)
        .await?
        .with_context(|| format!("Could not find user '{}'", config.twitch_channel))?
        .id;

    let redemptions_listen_command = twitch_api2::pubsub::listen_command(
        &[twitch_api2::pubsub::channel_points::ChannelPointsChannelV1 { channel_id: twitch_channel_id.as_str().parse::<u32>().unwrap() }.into_topic()],
        twitch_user_token.access_token.as_str(),
        "nonce"
    )?;


    let connect_to_pubsub = async || -> anyhow::Result<tokio_tungstenite::WebSocketStream<_>> {
        let (mut pubsub_connection, _) = tokio_tungstenite::connect_async(twitch_api2::TWITCH_PUBSUB_URL.clone()).await?;
        pubsub_connection.send(redemptions_listen_command.clone().into()).await?;
        log::info!("Connected to Twitch PubSub server.");
        Ok(pubsub_connection)
    };

    let pubsub_connection = Arc::new(tokio::sync::Mutex::new(connect_to_pubsub().await?));

    log::info!("Client running...");

    let pubsub_ping_connection = Arc::clone(&pubsub_connection);
    let ping_thread = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;

        loop {
            log::info!("Pinging Twitch PubSub server...");

            {
                let mut ping_connection = pubsub_ping_connection.lock().await;

                if let Err(e) = ping_connection.send(serde_json::to_string(&TwitchPubSubRequest { r#type: "PING".into() }).unwrap().into()).await {
                    log::warn!("Twitch PubSub ping failed: {}", anyhow::anyhow!(e));
                }
            }

            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    while let Some(message) = {
        let mut pubsub_connection = pubsub_connection.lock().await;
        let next_message = pubsub_connection.next().await;

        match next_message {
            Some(Err(e)) => {
                log::warn!("Getting next message failed: {}", anyhow::anyhow!(e));
                log::warn!("Reconnecting...");

                *pubsub_connection = connect_to_pubsub().await?;
                pubsub_connection.next().await
            },
            _ => next_message
        }
    } {
        let response = twitch_api2::pubsub::Response::parse(&message?.into_text()?)?;

        match response {
            twitch_api2::pubsub::Response::Message { data: twitch_api2::pubsub::TopicData::ChannelPointsChannelV1 { reply, .. } } => match *reply {
                twitch_api2::pubsub::channel_points::ChannelPointsChannelV1Reply::RewardRedeemed { redemption, timestamp } => {
                    let matching_mapping = config.redemption_mapping.iter().find(|mapping| mapping.name == redemption.reward.title);

                    log::info!(
                        "User '{}' redeemed reward '{}' at {}, mapped in config: {}",
                        redemption.user.login.as_str(),
                        redemption.reward.title,
                        timestamp,
                        matching_mapping.is_some()
                    );

                    if let Some(matching_mapping) = matching_mapping {
                        match &matching_mapping.action {
                            RedemptionMappingAction::ToggleFilterVisibility { source, filter } => {
                                let sources = obs_client.sources().get_sources_list().await?;
                                let matching_source = sources.iter()
                                    .find(|inner_source| &inner_source.name == source)
                                    .with_context(|| format!("Could not find source '{}'", source))?;

                                let filters = obs_client.sources().get_source_filters(&matching_source.name).await?;
                                let matching_filter = filters.iter()
                                    .find(|inner_filter| &inner_filter.name == filter)
                                    .with_context(|| format!("Could not find filter '{}'", filter))?;

                                obs_client.sources().set_source_filter_visibility(
                                    obws::requests::SourceFilterVisibility {
                                        source_name: &matching_source.name,
                                        filter_name: &matching_filter.name,
                                        filter_enabled: !matching_filter.enabled
                                    }
                                ).await?;

                                log::info!(
                                    "Changed visibility of filter '{}' of source '{}' to '{}'.",
                                    matching_source.name,
                                    matching_filter.name,
                                    !matching_filter.enabled
                                );
                            }
                        }
                    }
                }
                response => log::info!("Got channel points message: {:?}", response)
            }
            response => log::info!("Got web socket response: {:?}", response)
        }
    }


    ping_thread.await?;

    Ok(())
}