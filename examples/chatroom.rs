use std::collections::HashMap;
use ws_hotel::{
    CloseCode, Context, Message, Relocation, ResultRelocation, Room, RoomHandler, RoomRef,
    RoomRefWeak,
};

#[derive(Debug, Default)]
struct ChatRooms(HashMap<String, RoomRefWeak<ChatRoom>>);

impl ChatRooms {
    pub fn find_or_create_room(
        &mut self,
        name: &str,
        lobby: &RoomRefWeak<Lobby>,
    ) -> RoomRef<ChatRoom> {
        let rooms = &mut self.0;

        if let Some(weak) = rooms.get(name) {
            if let Some(strong) = weak.upgrade() {
                return strong;
            }
        }

        println!("creating new chatroom: {}", name);

        let room = Room::new(ChatRoom {
            lobby: lobby.clone(),
            name: name.into(),
            message: Vec::new(),
        });

        rooms.insert(String::from(name), room.downgrade());

        room
    }
}

#[derive(Debug)]
struct Lobby {
    rooms: ChatRooms,
}

impl Lobby {
    pub fn new() -> Self {
        Self {
            rooms: ChatRooms::default(),
        }
    }
}

impl RoomHandler for Lobby {
    type Guest = Option<String>;

    fn on_message(&mut self, mut cx: Context<Self>, msg: Message) -> ResultRelocation {
        let username = cx.identity();

        let msg = msg.as_text().unwrap_or_default();

        if username.is_none() {
            if let Some(username_msg) = msg.strip_prefix("/nick ") {
                println!("{:?} is joining", username_msg);
                *username = Some(username_msg.into());
            } else {
                cx.send("You haven't chosen a name yet")?;
            }

            return Ok(None);
        }

        if let Some(room) = msg.strip_prefix("/join ") {
            if let Some(username) = username.take() {
                let room = self.rooms.find_or_create_room(room, cx.room());
                let r = Relocation::new(&room, username);

                dbg!(&self.rooms);

                return Ok(Some(r));
            } else {
                cx.send("You're not logged in !")?;
            }
        } else {
            cx.send("Type `/join <room>` to join a chat room.")?;
        }

        Ok(None)
    }
}

#[derive(Debug)]
struct ChatRoom {
    lobby: RoomRefWeak<Lobby>,

    name: String,
    message: Vec<String>,
}

impl Drop for ChatRoom {
    fn drop(&mut self) {
        println!("dropping chatroom {}", &self.name);
    }
}

impl RoomHandler for ChatRoom {
    type Guest = String;

    fn on_join(&mut self, mut cx: Context<Self>) -> ResultRelocation {
        let message = format!("[SERVER]: {} entered the room", cx.identity().as_str());
        cx.broadcast(message)?;

        Ok(None)
    }

    fn on_message(&mut self, mut cx: Context<Self>, msg: Message) -> ResultRelocation {
        let msg = msg.as_text().unwrap_or_default();

        if msg == "/leave" {
            let name = Some(cx.identity().clone());
            let lobby = self.lobby.upgrade().expect("lobby was dropped");
            return Ok(Some(Relocation::from(Relocation::new(&lobby, name))));
        }

        let name = cx.identity().as_str();

        let message = format!("{}: {}", name, msg);
        cx.broadcast(&*message)?;
        self.message.push(message);

        Ok(None)
    }

    fn on_leave(&mut self, mut cx: Context<Self>, code_and_reason: Option<(CloseCode, &str)>) {
        let name = cx.identity().as_str();
        let message = format!(
            "[SERVER]: {} left the room (reason: {:?})",
            name, code_and_reason,
        );

        cx.broadcast(message).unwrap();
    }
}

pub fn main() {
    ws_hotel::listen("localhost:8080", Lobby::new());
}
