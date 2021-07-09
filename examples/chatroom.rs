use ws_hotel::{AdHoc, CloseCode, Context, Message, Room, RoomHandler};

#[derive(Debug)]
struct ChatRoom {
    name: String,
    message: Vec<String>,
}

impl RoomHandler for ChatRoom {
    type Guest = String;

    fn on_join(&mut self, mut cx: Context<Self::Guest>) -> ws_hotel::Result<()> {
        let message = format!("[SERVER]: {} entered the room", cx.identity().as_str());
        cx.broadcast(message)
    }

    fn on_message(&mut self, mut cx: Context<Self::Guest>, msg: Message) -> ws::Result<()> {
        let name = cx.identity().as_str();

        let message = format!("{}: {}", name, msg.as_text().unwrap_or_default());
        cx.broadcast(&*message)?;
        self.message.push(message);
        Ok(())
    }

    fn on_leave(
        &mut self,
        mut cx: Context<Self::Guest>,
        code_and_reason: Option<(CloseCode, &str)>,
    ) {
        let name = cx.identity().as_str();
        let message = format!(
            "[SERVER]: {} left the room (reason: {:?})",
            name, code_and_reason,
        );

        cx.broadcast(message).unwrap();
    }
}

#[derive(Debug, Default)]
struct ChatRooms(Vec<Room<ChatRoom>>);

impl ChatRooms {
    pub fn find_or_create_room(&mut self, name: &str) -> &Room<ChatRoom> {
        let rooms = &mut self.0;

        // Borrow checker is annoying if I do things more idiomatically
        // c.f. https://stackoverflow.com/a/58250510
        //    @ https://stackoverflow.com/users/3650362/trentcl

        if let Some(i) = (0..rooms.len()).find(|&i| rooms[i].with(|r| r.name == name)) {
            &mut rooms[i]
        } else {
            println!("creating new chatroom: {}", name);

            rooms.push(Room::new(ChatRoom {
                name: name.into(),
                message: Vec::new(),
            }));

            rooms.last().unwrap()
        }
    }
}

pub fn main() {
    let mut chat_rooms = ChatRooms::default();

    ws_hotel::listen(
        "localhost:8080",
        AdHoc::new(move |mut cx: Context<Option<String>>, msg: Message| {
            let username = cx.identity();

            let msg = msg.as_text().unwrap_or_default();

            if username.is_none() {
                return if let Some(username_msg) = msg.strip_prefix("/nick ") {
                    println!("{:?} is joining", username_msg);
                    *username = Some(username_msg.into());
                    Ok(())
                } else {
                    cx.send("You haven't chosen a name yet")
                };
            }

            if let Some(room) = msg.strip_prefix("/join ") {
                if let Some(username) = username.take() {
                    let room = chat_rooms.find_or_create_room(room);
                    cx.relocate(room, username);

                    dbg!(&chat_rooms);

                    Ok(())
                } else {
                    cx.send("You're not logged in !")
                }
            } else {
                cx.send("Type `/join <room>` to join a chat room.")
            }
        }),
    );
}
