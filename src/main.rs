use std::{borrow::Borrow, env, path::Path, process::exit};
use rand::Rng;
use rusqlite::{params, Connection, Result, Statement, OpenFlags};
use matrix_sdk::{
    config::SyncSettings,
    ruma::events::room::{
        member::StrippedRoomMemberEvent,
        message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEvent, RoomMessageEventContent},
    },
    Client, Room, RoomState,
};
use tokio::time::{sleep, Duration};
use chrono::prelude::*;


// Initializing i18
use rust_i18n::t;
#[macro_use]
extern crate rust_i18n;
i18n!("locales");

#[tokio::main]
async fn main() -> anyhow::Result<()> {

    rust_i18n::set_locale("en");

    // set up some simple stderr logging. You can configure it by changing the env
    // var `RUST_LOG`
    //tracing_subscriber::fmt::init();

    // parse the command line for homeserver, username and password
    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                // exit if missing
                exit(1)
            }
        };

    // our actual runner
    login_and_sync(homeserver_url, &username, &password).await?;
    Ok(())
}

// The core sync loop we have running.
async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let client = Client::builder()
        // We use the convenient client builder to set our custom homeserver URL on it.
        .homeserver_url(homeserver_url)
        .build()
        .await?;

    // Then let's log that client in
    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("getting started bot")
        .await?;
    println!("logged in as {username}");

    // Now, we want our client to react to invites. Invites sent us stripped member
    // state events so we want to react to them. We add the event handler before
    // the sync, so this happens also for older messages. All rooms we've
    // already entered won't have stripped states anymore and thus won't fire
    client.add_event_handler(on_stripped_state_member);

    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    let sync_response = client.sync_once(SyncSettings::default()).await.unwrap();
    let sync_token = sync_response.next_batch;

    // now that we've synced, let's attach a handler for incoming room messages, so
    // we can react on it
    client.add_event_handler(on_room_message);

    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings: SyncSettings = SyncSettings::default().token(sync_token);
    // this keeps state from the server streaming in to the bot via the
    // EventHandler trait
    client.sync(settings).await?; // this essentially loops until we kill the bot

    Ok(())
}

// Whenever we see a new stripped room member event, we've asked our client to
// call this function. So what exactly are we doing then?
async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        // the invite we've seen isn't for us, but for someone else. ignore
        return;
    }

    // The event handlers are called before the next sync begins, but
    // methods that change the state of a room (joining, leaving a room)
    // wait for the sync to return the new room state so we need to spawn
    // a new task for them.
    tokio::spawn(async move {
        println!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.join().await {
            // retry autojoin due to synapse sending invites, before the
            // invited user can join for more information see
            // https://github.com/matrix-org/synapse/issues/4345
            eprintln!("Failed to join room {} ({err:?}), retrying in {delay}s", room.room_id());

            sleep(Duration::from_secs(delay)).await;
            delay *= 2;

            if delay > 3600 {
                eprintln!("Can't join room {} ({err:?})", room.room_id());
                break;
            }
        }
        println!("Successfully joined room {}", room.room_id());
    });
}

// This fn is called whenever we see a new room message event
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room, client: Client) -> anyhow::Result<()> {
    // First, we need to unpack the message: We only want messages from rooms we are
    // still in and that are regular text messages - ignoring everything else.
    if room.state() != RoomState::Joined {
        return Ok(());
    }
    // Then, we're assuring that the event we're dealing with has not been sent by us
    if event.sender == client.user_id().unwrap() {
        return Ok(());
    }

    let conn = initialize_db("./db1.db");


    let mut bot_user: BotUser = match conn.query_row(
        "SELECT matrix_user FROM bot_users WHERE matrix_user=?1", 
        params![&event.sender.to_string()],
        |row| row.get::<usize, String>(0)
    ) {
        Ok(_) => BotUser::user_from_db_row(event.sender.to_string(), &conn),
        Err(_) => {
            let bot_user: BotUser = BotUser::initialize_user(event.sender.to_string());
            conn.execute(
                "INSERT INTO bot_users (matrix_user, ticket_room, attached_to, is_admin, is_banned) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![&bot_user.matrix_user, &bot_user.ticket_room, &bot_user.attached_to, &bot_user.is_admin, &bot_user.is_banned],
            ).unwrap();
            bot_user
        }
    };

    let MessageType::Text(text_content) = event.content.msgtype else { return Ok(()) };


    if text_content.body.starts_with("!help") {
        room.send(
            RoomMessageEventContent::text_html_auto(
                t!("commands.help.message")
            )
        ).await.unwrap();
    }

    if text_content.body.starts_with("!chat") {
        match text_content.body.args() {
            None => {
                room.send(
                    RoomMessageEventContent::text_html_auto(
                        t!("commands.chat.usage")
                    )
                ).await.unwrap();
            }
            Some(_) => {
                let arguments: Vec<String> = text_content.body.args().unwrap();
                println!("{:?}", arguments);
                let mut options: ChatCommandOptions = ChatCommandOptions::new();
                for arg in arguments {
                    if arg.starts_with("-"){
                        if arg == "-A" || arg == "--anonymous" {
                            options.anonymous = true
                        } else if arg == "-F" || arg == "--fuckanons" {
                            options.fuckanons = true
                        } else if arg == "-T" || arg == "--ticket" {
                            options.ticket = true
                        } else { }
                    }
                    if arg == "create" {
                            if bot_user.is_admin {
                                let new_chat: FluxChatRoom = FluxChatRoom::admin_new(&bot_user.matrix_user, &conn).unwrap();
                                if bot_user.attached_to != "" {
                                    () // TODO this should return a gigantic error because should be an unreachable point of the code,
                                       // No user should be in fact able to use any bot command while attached to a chat!
                                } else {
                                    bot_user.attached_to = new_chat.id.to_string()
                                }
                                bot_user.push_to_db(&conn).unwrap();
                                new_chat.push_to_db(&conn).unwrap();
                                room.send(RoomMessageEventContent::text_html_auto(
                                    t!("commands.chat.create.success", id=&new_chat.id))).await.unwrap();
                        } else {
                            if options.ticket {
                                () // Create a ticket room
                            } else {
                                // Send error message, you must specify -T option, or create the room and then 
                                // tell them it's a ticket room
                            }
                        }
                    } else 
                    if arg == "attach" {
                        ()
                    } else 
                    if arg == "delete" {
                        ()
                    } else 
                    if arg == "close" {
                        ()
                    }
                }
                if options.has_options() {
                    println!("We got options!")
                }
            }
        }

    
    }


    // The first time we run the bot we DEFINITELY WANT to issue this command to take the bot ownership
    if text_content.body.starts_with("!sumyself") {
        let sumyself = conn.query_row("SELECT used FROM sumyself", (), |row| row.get::<usize, bool>(0)).unwrap();
        if sumyself {
            // Do nothing
        } else {
            conn.execute("UPDATE sumyself SET used=1 WHERE used=0", ()).unwrap();
            bot_user.is_admin = true;
            bot_user.push_to_db(&conn).unwrap();
            room.send(RoomMessageEventContent::text_html_auto(
                t!("commands.sumyself.success")
            )).await.unwrap();
        }
    }

    Ok(())
}

pub trait HtmlContentOnce {
    fn text_html_auto(txt: impl AsRef<str>) -> RoomMessageEventContent;

    // TODO redo this as const fn or macro for comptime optimization
    fn strip(txt: &str) -> String {
        let frag = scraper::Html::parse_fragment(txt);
        let mut out = String::new();
        for node in frag.tree {
            if let scraper::node::Node::Text(text) = node {
                out.push_str(&text.text.to_string())
            }
        }
        out
    }
}

impl HtmlContentOnce for RoomMessageEventContent {
    fn text_html_auto(txt: impl AsRef<str>) -> RoomMessageEventContent {
        RoomMessageEventContent::text_html(Self::strip(txt.as_ref()), txt.as_ref())
    }
}


/// Converts a string into a vector of strings, I'm feeling dirty for doing this...
pub trait ParseArguments {
    fn args(&self) -> Option<Vec<String>>;
}

impl ParseArguments for String {
    fn args(&self) -> Option<Vec<String>> {
        let have_args: Option<usize> = self.find(" ");
        if have_args.is_some() {
            let command_args: Vec<String> = self.split_whitespace().map(str::to_string).collect();
            Some(command_args)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
struct BotUser {
    matrix_user: String,
    ticket_room: String,
    attached_to: String,
    is_admin: bool,
    is_banned: bool
}

impl BotUser {
    /// Initialize an user struct with default data and the User ID
    fn initialize_user(matrix_user: String) -> BotUser {
        BotUser {
            matrix_user: matrix_user,
            ticket_room: String::new(),
            attached_to: String::new(),
            is_admin: false,
            is_banned: false
        }
    }

    /// Retrieve the user's struct for an user already in database
    fn user_from_db_row(matrix_user: String, conn: &Connection) -> BotUser {
        let mut stmt: Statement = conn.prepare("SELECT * FROM bot_users WHERE matrix_user=?1").unwrap();
        BotUser {
            matrix_user: (&matrix_user).to_owned(),
            ticket_room: stmt.query_row(params![&matrix_user], |val| val.get(1)).unwrap(),
            attached_to: stmt.query_row(params![&matrix_user], |val| val.get(2)).unwrap(),
            is_admin: stmt.query_row(params![&matrix_user], |val| val.get::<usize, bool>(3)).unwrap(),
            is_banned: stmt.query_row(params![&matrix_user], |val| val.get::<usize, bool>(4)).unwrap(),

        } 
    }


    /// Updates the database with new bot User values
    /// 
    /// Please, take note that matrix_user should 
    /// never change, therefore it won't be pushed to the database.
    /// 
    /// Returns the passed struct on success
    fn push_to_db(&self, conn: &Connection) -> anyhow::Result<BotUser> {
        conn.execute(
            "UPDATE bot_users
            SET ticket_room=?1, attached_to=?2, is_admin=?3, is_banned=?4
            WHERE matrix_user=?5",
        params![&self.ticket_room, &self.attached_to, &self.is_admin, &self.is_banned, &self.matrix_user])?;
        Result::Ok(self.clone())
    }
}


struct ChatCommandOptions {
    anonymous: bool,
    fuckanons: bool,
    ticket: bool
}

impl ChatCommandOptions {
    fn new() -> ChatCommandOptions {
        ChatCommandOptions{
            anonymous: false,
            fuckanons: false,
            ticket: false
        }
    }

    fn has_options(&self) -> bool {
        if self.anonymous || self.fuckanons || self.ticket {
            true
        } else {false}
    }
}


//TODO What happens if an user attaches to a chat as an anon and as normal?
#[derive(Debug, Clone)]
struct FluxChatUser {
    matrix_user: String,
    display_name: String,
    is_anon: bool,
    chat_id: String
}

#[derive(Debug, Clone)]
struct FluxChatRoom {
    id: String,
    creation_date: String,
    creator: String,
    is_closed: bool
}

impl FluxChatRoom {
    /// Initializes a FluxChatRoom struct and pushes it to the database, returns the newly
    /// created struct (or error, Result<FluxChatRoom, E>)
    fn admin_new(creator: impl ToString, conn: &Connection) -> anyhow::Result<FluxChatRoom> {
        let mut generator = rand::thread_rng();
        let mut chat_id: String = (0..4).map(|_| char::from(generator.gen_range(65..=90))).collect();
        chat_id.push('-');
        chat_id.push_str(&((0..4).map(|_| char::from(generator.gen_range(65..=90))).collect::<String>()));

        let current_date = Local::now();
        let current_date_string = current_date.format("%Y-%m-%d %H:%M:%S").to_string();
        conn.execute(
            "INSERT INTO flux_chat_rooms (id, creation_date, creator, is_closed) VALUES (?1, ?2, ?3, ?4)",
            params![&chat_id, &current_date_string, creator.to_string(), false]
        )?;
        Result::Ok(FluxChatRoom {
            id: chat_id,
            creation_date: current_date_string,
            creator: creator.to_string(),
            is_closed: false
        })
    }

    /// Updates the database with new FluxChatRoom values
    /// 
    /// Please, take note that any value that is
    /// not is_closed shouldn't be changed since Chat creation so those values won't be pushed.
    /// 
    /// Returns the inserted struct on success
    fn push_to_db(&self, conn: &Connection) -> anyhow::Result<FluxChatRoom> {
        conn.execute(
            "UPDATE flux_chat_rooms
            SET is_closed=?2
            WHERE id=?1",
        params![&self.id, &self.is_closed])?;
        Result::Ok(self.clone())
    }
}


/// Connects to pre-existing database or generates a new working one :3
/// 
/// Without this function we basically can't get the bot to work, in fact
/// this function role is to connect to an existing database or initialize one
/// with the tables needed for the bot to work. Always use this to generate new databases
/// or generate manually the tables below
pub fn initialize_db(path: impl AsRef<Path>) -> Connection {
    let conn = match Connection::open_with_flags(&path, {
        OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI
    }) {
        Ok(val) => {
            println!("Database found! Loading...");
            val
        },
        Err(_) => {
            println!("Database not found! Generating a new one :3");
            let temp_conn = Connection::open(&path).unwrap();
            temp_conn.execute(
                "CREATE TABLE bot_users (
                    matrix_user TEXT NOT NULL,
                    ticket_room TEXT,
                    attached_to TEXT,
                    is_admin INTEGER,
                    is_banned INTEGER
                )",
                ()).unwrap();
            temp_conn.execute(
                "CREATE TABLE flux_chat_rooms (
                    id TEXT NOT NULL,
                    creation_date TEXT NOT NULL,
                    creator TEXT NOT NULL,
                    is_closed INTEGER
                )", ()).unwrap();
            temp_conn.execute(
                "CREATE TABLE flux_chat_users (
                    matrix_user TEXT NOT NULL,
                    display_name TEXT NOT NULL,
                    is_anon INTEGER,
                    chat_id TEXT NOT NULL
                )", ()).unwrap();
            temp_conn.execute(
                "CREATE TABLE sumyself (
                    used INTEGER
                )", ()
            ).unwrap();
            temp_conn.execute("INSERT INTO sumyself (used) VALUES (0)", ()).unwrap();
            temp_conn
        }
    };
    conn
}
