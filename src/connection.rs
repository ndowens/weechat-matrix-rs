use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    rc::{Rc, Weak},
    time::Duration,
};

use async_std::sync::{channel as async_channel, Receiver, Sender};
use serde_json::json;
use tokio::runtime::Runtime;
use tracing::{error, info};
use uuid::Uuid;

use matrix_sdk::{
    self,
    api::r0::{
        device::{
            delete_devices::Response as DeleteDevicesResponse,
            get_devices::Response as DevicesResponse,
        },
        filter::{
            FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter,
        },
        membership::get_member_events::Response as MembersResponse,
        message::{
            get_message_events::{
                Request as MessagesRequest, Response as MessagesResponse,
            },
            send_message_event::Response as RoomSendResponse,
        },
        session::login::Response as LoginResponse,
        sync::sync_events::Filter,
        typing::create_typing_event::{Response as TypingResponse, Typing},
        uiaa::AuthData,
    },
    events::{
        room::{
            member::MemberEventContent,
            message::{MessageEventContent, TextMessageEventContent},
        },
        AnyMessageEventContent, AnySyncRoomEvent, AnySyncStateEvent,
        StateEvent,
    },
    identifiers::{DeviceIdBox, RoomId, UserId},
    locks::RwLock,
    Client, ClientConfig, LoopCtrl, Result as MatrixResult, Room, SyncSettings,
};

use weechat::{Task, Weechat};

use crate::server::{InnerServer, MatrixServer};

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);
pub const TYPING_NOTICE_TIMEOUT: Duration = Duration::from_secs(4);

pub struct InteractiveAuthInfo {
    pub user: String,
    pub password: String,
    pub session: Option<String>,
}

impl InteractiveAuthInfo {
    pub fn as_auth_data(&self) -> AuthData<'_> {
        let mut auth_parameters = BTreeMap::new();
        let identifier = json!({
            "type": "m.id.user",
            "user": self.user,
        });

        auth_parameters.insert("identifier".to_owned(), identifier);
        auth_parameters
            .insert("password".to_owned(), self.password.clone().into());

        // This is needed because of https://github.com/matrix-org/synapse/issues/5665
        auth_parameters.insert("user".to_owned(), self.user.clone().into());

        AuthData::DirectRequest {
            kind: "m.login.password",
            auth_parameters,
            session: self.session.as_deref(),
        }
    }
}

pub enum ClientMessage {
    LoginMessage(LoginResponse),
    SyncState(RoomId, AnySyncStateEvent),
    SyncEvent(RoomId, AnySyncRoomEvent),
    Members(RoomId, MembersResponse),
    RestoredRoom(Room),
}

/// Struc representing an active connection to the homeserver.
///
/// Since the rust-sdk `Client` object uses reqwest for the HTTP client making
/// requests requires the request to be made on a tokio runtime. The connection
/// wraps the `Client` object and makes sure that requests are run on the
/// runtime the `Connection` holds.
///
/// While this struct is alive a sync loop will be going on. To cancel the sync
/// loop drop the object.
#[derive(Debug, Clone)]
pub struct Connection {
    #[used]
    receiver_task: Rc<Task<()>>,
    client: Client,
    pub runtime: Rc<Runtime>,
}

impl Connection {
    pub async fn spawn<F>(&self, future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime
            .spawn(future)
            .await
            .expect("Tokio error while sending a message")
    }

    pub fn new(server: &MatrixServer, client: &Client) -> Self {
        let (tx, rx) = async_channel(1000);

        let server_name = server.name();

        let receiver_task = Weechat::spawn(Connection::response_receiver(
            rx,
            server.clone_inner_weak(),
        ));

        let server = server.inner();

        let runtime = Runtime::new().unwrap();

        let settings = server.settings();

        runtime.spawn(Connection::sync_loop(
            client.clone(),
            tx,
            settings.username.to_string(),
            settings.password.to_string(),
            server_name.to_string(),
            server.get_server_path(),
        ));

        Self {
            client: client.clone(),
            runtime: Rc::new(runtime),
            receiver_task: Rc::new(receiver_task),
        }
    }

    /// Send a message to the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room which the message should be sent to.
    ///
    /// * `content` - The content of the message that will be sent to the
    /// server.
    ///
    /// * `transaction_id` - Attach an unique id to this message, later on the
    /// event will contain the same id in the unsigned part of the event.
    pub async fn send_message(
        &self,
        room_id: &RoomId,
        content: AnyMessageEventContent,
        transaction_id: Option<Uuid>,
    ) -> MatrixResult<RoomSendResponse> {
        let room_id = room_id.to_owned();
        let client = self.client.clone();

        self.spawn(async move {
            client
                .room_send(
                    &room_id,
                    content,
                    Some(transaction_id.unwrap_or_else(Uuid::new_v4)),
                )
                .await
        })
        .await
    }

    pub async fn delete_devices(
        &self,
        devices: Vec<DeviceIdBox>,
        auth_info: Option<InteractiveAuthInfo>,
    ) -> MatrixResult<DeleteDevicesResponse> {
        let client = self.client.clone();
        self.spawn(async move {
            if let Some(info) = auth_info {
                let auth = Some(info.as_auth_data());
                client.delete_devices(&devices, auth).await
            } else {
                client.delete_devices(&devices, None).await
            }
        })
        .await
    }

    /// Fetch historical messages for the given room.
    pub async fn room_messages(
        &self,
        room_id: RoomId,
        prev_batch: String,
    ) -> MatrixResult<MessagesResponse> {
        let client = self.client.clone();

        self.spawn(async move {
            let request = MessagesRequest::backward(&room_id, &prev_batch);
            client.room_messages(request).await
        })
        .await
    }

    /// Get the list of our own devices.
    pub async fn devices(&self) -> MatrixResult<DevicesResponse> {
        let client = self.client.clone();
        self.spawn(async move { client.devices().await }).await
    }

    /// Set or reset a typing notice.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room where the typing notice should be
    /// active.
    ///
    /// * `typing` - Should we set or unset the typing notice.
    pub async fn send_typing_notice(
        &self,
        room_id: &RoomId,
        typing: bool,
    ) -> MatrixResult<TypingResponse> {
        let room_id = room_id.to_owned();
        let client = self.client.clone();

        self.spawn(async move {
            let typing = if typing {
                Typing::Yes(TYPING_NOTICE_TIMEOUT)
            } else {
                Typing::No
            };

            client.typing_notice(&room_id, typing).await
        })
        .await
    }

    fn save_device_id(
        user_name: &str,
        mut server_path: PathBuf,
        response: &LoginResponse,
    ) -> std::io::Result<()> {
        server_path.push(user_name);
        server_path.set_extension("device_id");
        std::fs::write(&server_path, &response.device_id.to_string())
    }

    fn load_device_id(
        user_name: &str,
        mut server_path: PathBuf,
    ) -> std::io::Result<Option<String>> {
        server_path.push(user_name);
        server_path.set_extension("device_id");

        let device_id = std::fs::read_to_string(server_path);

        if let Err(e) = device_id {
            // A file not found error is ok, report the rest.
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e);
            }
            return Ok(None);
        };

        let device_id = device_id.unwrap_or_default();

        if device_id.is_empty() {
            Ok(None)
        } else {
            Ok(Some(device_id))
        }
    }

    /// Response receiver loop.
    /// This runs on the main Weechat thread and listens for responses coming
    /// from the client running in the tokio executor.
    pub async fn response_receiver(
        receiver: Receiver<Result<ClientMessage, String>>,
        server: Weak<RefCell<InnerServer>>,
    ) {
        loop {
            let ret = receiver.recv().await;

            let server_cell = server
                .upgrade()
                .expect("Can't upgrade server in sync receiver");
            let mut server = server_cell.borrow_mut();

            let message = match ret {
                Ok(m) => m,
                Err(e) => {
                    server.print_error(&format!(
                        "Error receiving message: {:?}",
                        e
                    ));
                    return;
                }
            };

            match message {
                Ok(message) => match message {
                    ClientMessage::LoginMessage(r) => server.receive_login(r),
                    ClientMessage::SyncEvent(r, e) => {
                        server.receive_joined_timeline_event(&r, e).await
                    }
                    ClientMessage::SyncState(r, e) => {
                        server.receive_joined_state_event(&r, e).await
                    }
                    ClientMessage::RestoredRoom(room) => {
                        server.restore_room(room).await
                    }
                    ClientMessage::Members(room, e) => {
                        server.receive_members(&room, e).await
                    }
                },
                Err(e) => server.print_error(&format!("Ruma error {}", e)),
            };
        }
    }

    fn sync_filter() -> FilterDefinition<'static> {
        FilterDefinition {
            room: RoomFilter {
                state: RoomEventFilter {
                    lazy_load_options: LazyLoadOptions::Enabled {
                        include_redundant_members: false,
                    },
                    limit: Some(10u16.into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Main client sync loop.
    /// This runs on the per server tokio executor.
    /// It communicates with the main Weechat thread using a async channel.
    pub async fn sync_loop(
        client: Client,
        channel: Sender<Result<ClientMessage, String>>,
        username: String,
        password: String,
        server_name: String,
        server_path: PathBuf,
    ) {
        if !client.logged_in().await {
            let device_id =
                Connection::load_device_id(&username, server_path.clone());

            let device_id = match device_id {
                Err(e) => {
                    channel
                        .send(Err(format!(
                        "Error while reading the device id for server {}: {:?}",
                        server_name, e
                    )))
                        .await;
                    return;
                }
                Ok(d) => d,
            };

            let first_login = device_id.is_none();

            let ret = client
                .login(
                    &username,
                    &password,
                    device_id.as_deref(),
                    Some("Weechat-Matrix-rs"),
                )
                .await;

            match ret {
                Ok(response) => {
                    if let Err(e) = Connection::save_device_id(
                        &username,
                        server_path.clone(),
                        &response,
                    ) {
                        channel
                            .send(Err(format!(
                            "Error while writing the device id for server {}: {:?}",
                            server_name, e
                        ))).await;
                        return;
                    }

                    channel
                        .send(Ok(ClientMessage::LoginMessage(response)))
                        .await
                }
                Err(e) => {
                    channel
                        .send(Err(format!("Failed to log in: {:?}", e)))
                        .await;
                    return;
                }
            }

            // if !first_login {
            //     let joined_rooms = client.joined_rooms();
            //     for room in joined_rooms.read().await.values() {
            //         channel
            //             .send(Ok(ClientMessage::RestoredRoom(room.clone())))
            //             .await
            //     }
            // }
        }

        let filter = client
            .get_or_upload_filter("sync", Connection::sync_filter())
            .await
            .unwrap();

        let sync_token = client.sync_token().await;
        let sync_settings = SyncSettings::new()
            .timeout(DEFAULT_SYNC_TIMEOUT)
            .filter(Filter::FilterId(&filter));

        let sync_settings = if let Some(t) = sync_token {
            sync_settings.token(t)
        } else {
            sync_settings
        };

        let sync_channel = &channel;

        let client_ref = &client;

        client
            .sync_with_callback(sync_settings, |response| async move {
                for (room_id, room) in response.rooms.join {
                    for event in room.state.events {
                        sync_channel
                            .send(Ok(ClientMessage::SyncState(
                                room_id.clone(),
                                event,
                            )))
                            .await;
                    }
                    for event in room.timeline.events {
                        sync_channel
                            .send(Ok(ClientMessage::SyncEvent(
                                room_id.clone(),
                                event,
                            )))
                            .await;
                    }

                    if let Some(r) = client_ref.get_joined_room(&room_id) {
                        if !r.are_members_synced() {
                            let room_id = room_id.clone();
                            let client = client_ref.clone();
                            let channel = sync_channel.clone();

                            tokio::spawn(async move {
                                if let Ok(members) =
                                    client.room_members(&room_id).await
                                {
                                    channel
                                        .send(Ok(ClientMessage::Members(
                                            room_id.clone(),
                                            members,
                                        )))
                                        .await;
                                }
                            });
                        }
                    }
                }

                LoopCtrl::Continue
            })
            .await;
    }
}
