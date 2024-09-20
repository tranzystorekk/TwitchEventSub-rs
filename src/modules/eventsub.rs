use log::warn;
use std::{
  io::ErrorKind,
  net::TcpStream,
  sync::mpsc::Sender as SyncSender,
  thread,
  time::{Duration, Instant},
};

use log::{error, info};
use tungstenite::{connect, stream::MaybeTlsStream, Error, Message as NetworkMessage, WebSocket};
use twitch_eventsub_structs::{Event, EventMessageType, GenericMessage, Subscription};

use crate::{
  EventSubError, ResponseType, TokenAccess, TwitchEventSubApi, TwitchHttpRequest, TwitchKeys,
  CONNECTION_EVENTS, SUBSCRIBE_URL,
};

use super::irc_bot::IRCChat;

#[cfg(feature = "only_raw_responses")]
pub fn events(
  client: WebSocket<MaybeTlsStream<TcpStream>>,
  message_sender: SyncSender<ResponseType>,
  subscriptions: Vec<Subscription>,
  mut custom_subscriptions: Vec<String>,
  twitch_keys: TwitchKeys,
) {
  loop {
    let message = match client.read() {
      Ok(m) => m,
      Err(Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
        continue;
      }
      Err(e) => {
        error!("recv message error: {:?}", e);
        let _ = client.send(NetworkMessage::Close(None));
        let _ = message_sender.send(ResponseType::Close);

        return;
      }
    };

    if let NetworkMessage::Text(msg) = message.clone() {
      let _ = message_sender.send(ResponseType::RawResponse(msg));
      continue;
    }
  }
}

#[cfg(not(feature = "only_raw_responses"))]
pub fn events(
  mut client: WebSocket<MaybeTlsStream<TcpStream>>, //Client<TlsStream<TcpStream>>>>,
  message_sender: SyncSender<ResponseType>,
  subscriptions: Vec<Subscription>,
  mut custom_subscriptions: Vec<String>,
  mut twitch_keys: TwitchKeys,
  save_locations: Option<(String, String)>,
  mut irc: Option<IRCChat>,
) {
  use std::sync::mpsc::channel;

  use crate::modules::irc_bot::{IRCMessage, IRCResponse};

  use super::irc_bot;

  let mut last_message = Instant::now();

  let mut is_reconnecting = false;

  let mut messages_from_irc = None;
  if let Some(irc) = irc {
    let (transmit_messages, receive_message) = channel();

    let receive_thread = thread::spawn(move || {
      irc_bot::irc_thread(irc, transmit_messages);
    });

    messages_from_irc = Some(receive_message);
  }

  let mut irc_messages: Vec<(Instant, IRCMessage)> = Vec::new();

  loop {
    let message = match client.read() {
      Ok(m) => m,
      Err(Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
        continue;
      }
      Err(e) => {
        error!("recv message error: {:?}", e);
        let _ = client.send(NetworkMessage::Close(None));
        let _ = message_sender.send(ResponseType::Close);
        continue;
      }
    };

    if let Some(irc_reciever) = &messages_from_irc {
      loop {
        match irc_reciever.recv_timeout(Duration::ZERO) {
          Ok(IRCResponse::IRCMessage(msg)) => irc_messages.push((Instant::now(), msg)),
          _ => break,
        }
      }
    }

    if last_message.elapsed().as_secs() > 60 {
      let _ = client.send(NetworkMessage::Close(None));
      thread::sleep(Duration::from_secs(1));
      println!("Messages not sent within the keep alive timeout restarting websocket");
      info!("Messages not sent within the keep alive timeout restarting websocket");
      let (new_client, _) = connect(CONNECTION_EVENTS)
        .expect("Failed to reconnect to new url after receiving reconnect message from twitch");
      client = new_client;
      last_message = Instant::now();
      is_reconnecting = true;
      continue;
    }

    match message {
      NetworkMessage::Text(msg) => {
        let message = serde_json::from_str(&msg);

        if let Err(e) = message {
          error!("Unimplemented twitch response: {}\n{}", msg, e);
          let _ = message_sender.send(ResponseType::RawResponse(msg));
          continue;
        }

        let message: GenericMessage = message.unwrap();

        match message.event_type() {
          EventMessageType::Welcome => {
            info!("EventSub: Welcome message!");
            let session_id = message.clone().payload.unwrap().session.unwrap().id;

            if !is_reconnecting {
              let mut sub_data = subscriptions
                .iter()
                .filter_map(|s| s.construct_data(&session_id, &twitch_keys.broadcaster_account_id))
                .filter_map(|s| serde_json::to_string(&s).ok())
                .collect::<Vec<_>>();
              sub_data.append(&mut custom_subscriptions);

              info!("Subscribing to events!");
              let mut clone_twitch_keys = twitch_keys.clone();
              if let Some(TokenAccess::User(ref token)) = twitch_keys.access_token {
                sub_data
                  .iter()
                  .map(|sub_data| {
                    TwitchHttpRequest::new(SUBSCRIBE_URL)
                      .full_auth(token.to_owned(), twitch_keys.client_id.to_string())
                      .json_content()
                      .is_post(sub_data)
                      .run()
                  })
                  .map(|a| {
                    TwitchEventSubApi::regen_token_if_401(
                      a,
                      &mut clone_twitch_keys,
                      &save_locations,
                    )
                  })
                  .filter_map(Result::err)
                  .for_each(|error| {
                    error!("{:?}", error);
                    message_sender
                      .send(ResponseType::Error(error))
                      .expect("Failed to send error Message back to main thread.");
                  });
              } else {
                let _ = message_sender.send(ResponseType::Error(
                  EventSubError::InvalidAccessToken(format!(
                    "Expected TokenAccess::User(TOKENHERE) but found {:?}",
                    twitch_keys.access_token
                  )),
                ));
              }

              twitch_keys = clone_twitch_keys;
              message_sender
                .send(ResponseType::Ready)
                .expect("Failed to send ready back to main thread.");
            }
            is_reconnecting = false;
            last_message = Instant::now();
          }
          EventMessageType::KeepAlive => {
            info!("Keep alive: {}", last_message.elapsed().as_secs());
            last_message = Instant::now();
          }
          EventMessageType::Reconnect => {
            println!("Reconnecting to Twitch!");
            info!("Reconnecting to Twitch!");
            let url = message
              .clone()
              .payload
              .unwrap()
              .session
              .unwrap()
              .reconnect_url
              .unwrap();

            is_reconnecting = true;
            let _ = client.send(NetworkMessage::Close(None));
            let (new_client, _) = connect(&url).expect(
              "Failed to reconnect to new url after recieving reocnnect message from twitch.",
            );
            client = new_client;
          }
          EventMessageType::Notification => {
            last_message = Instant::now();
            let mut message = message.payload.unwrap().event.unwrap();

            match &mut message {
              Event::ChatMessage(ref mut msg) => {
                let mut start = Instant::now();

                let mut old_messages = Vec::new();
                for (i, (time, irc_message)) in irc_messages.iter().enumerate() {
                  if irc_message.display_name == msg.chatter.name
                    && irc_message.message.contains(&msg.message.text)
                  {
                    msg.returning_chatter = irc_message.returning_chatter;
                    msg.first_time_chatter = irc_message.first_time_chatter;
                    msg.moderator = irc_message.moderator;
                    break;
                  }

                  if time.elapsed().as_secs() > 3 {
                    old_messages.push(i);
                  }
                }

                for i in old_messages.drain(..).rev() {
                  irc_messages.remove(i);
                }
              }
              _ => {}
            }

            let _ = message_sender.send(ResponseType::Event(message));
          }
          EventMessageType::Unknown => {
            last_message = Instant::now();
            //if !custom_subscriptions.is_empty() {
            let _ = message_sender.send(ResponseType::RawResponse(msg));
            //}
          }
        }
      }
      NetworkMessage::Close(a) => {
        warn!("Close message received: {:?}", a);
        // Got a close message, so send a close message and return
        let _ = client.send(NetworkMessage::Close(None));
        let _ = message_sender.send(ResponseType::Close);
        continue;
      }
      NetworkMessage::Ping(_) => {
        match client.send(NetworkMessage::Pong(Vec::new())) {
          // Send a pong in response
          Ok(()) => {}
          Err(e) => {
            error!("Received an Error from Server: {:?}", e);
            return;
          }
        }
      }
      _ => {}
    }
  }
}
