#![recursion_limit = "512"]

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use futures::{future, select};
use futures_util::{future::FutureExt, sink::SinkExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio_util::codec::{Decoder, Framed};

use rtalk_codec::{Event, EventCodec};

pub struct User {
    name: Option<String>,
    ip: std::net::SocketAddr,
    sender: Sender<Event>,
}

impl User {
    fn get_name(&self) -> String {
        match self.name.as_ref() {
            Some(name) => format!("{} [{:?}]", name, self.ip),
            None => format!("anonymous [{}]", self.ip),
        }
    }
}

struct State {
    counter: u64,
    users: BTreeMap<u64, User>,
}

type ClientConnection = Pin<Box<Framed<TcpStream, EventCodec>>>;

impl State {
    fn add_user(
        &mut self,
        session: Session,
        ip: std::net::SocketAddr,
        mut network: ClientConnection,
    ) -> u64 {
        self.counter += 1;

        let id = self.counter;

        let (sender, mut rx) = mpsc::channel::<Event>(100);

        let _task = tokio::spawn(async move {
            loop {
                select! {

                    // from session to network
                    event = rx.next().fuse() => {
                        if let Some(event) = event {
                            network.send(event).await.expect("Message send failed.");
                        }
                    },

                    // from network
                    event = network.next().fuse() => {
                        if let Some(Ok(event)) = event {
                            match event {
                                Event::RequestJoin(name) => {
                                    let name: String = session.update_user(id, name.clone());
                                    session.broadcast(|| Event::Joined(name.clone())).await;
                                }
                                Event::Leave() => {
                                    let name = session.remove_user(id);
                                    session.broadcast(|| Event::Left(name.clone())).await;
                                    break;
                                }
                                Event::MessageSend(msg) => {
                                    let who = session.get_name(id);
                                    session.broadcast(|| Event::MessageReceived(who.clone(), msg.clone())).await;
                                }
                                _ => unimplemented!()
                            }
                        }
                    }
                    complete => break,
                }
            }
        });

        self.users.insert(
            self.counter,
            User {
                name: None,
                ip,
                sender,
            },
        );

        self.counter
    }

    fn get_name(&self, id: u64) -> String {
        let user = self.users.get(&id).unwrap();
        user.get_name()
    }

    fn update_user(&mut self, id: u64, name: String) -> String {
        let user = self.users.get_mut(&id).unwrap();
        user.name = Some(name);
        user.get_name()
    }
}

#[derive(Clone)]
pub struct Session {
    state: Arc<RwLock<State>>,
}

impl Session {
    fn new() -> Self {
        Session {
            state: Arc::new(RwLock::new(State {
                counter: 0,
                users: BTreeMap::new(),
            })),
        }
    }

    fn add_user(&self, ip: std::net::SocketAddr, connection: ClientConnection) -> u64 {
        self.state
            .write()
            .unwrap()
            .add_user(self.clone(), ip, connection)
    }

    fn get_name(&self, id: u64) -> String {
        self.state.read().unwrap().get_name(id)
    }

    fn update_user(&self, id: u64, name: String) -> String {
        self.state.write().unwrap().update_user(id, name)
    }

    fn remove_user(&self, id: u64) -> String {
        let user = self.state.write().unwrap().users.remove(&id).unwrap();
        user.get_name()
    }

    fn user_ids(&self) -> Vec<u64> {
        self.state
            .read()
            .unwrap()
            .users
            .iter()
            .map(|(id, _)| *id)
            .collect()
    }

    async fn broadcast<F: Fn() -> Event>(&self, event_gen: F) {
        let futs = self
            .user_ids()
            .into_iter()
            .map(|dest_id| self.send_event(dest_id, event_gen()));
        future::join_all(futs).await;
    }

    async fn send_event(&self, id: u64, evt: Event) {
        let mut sender = {
            let state = self.state.read().unwrap();
            if let Some(user) = state.users.get(&id) {
                user.sender.clone()
            } else {
                return;
            }
        };

        sender
            .send(evt)
            .await
            .expect("Could not queue event to send");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let session = Session::new();

    let mut listener = TcpListener::bind("127.0.0.1:3215").await?;
    loop {
        let (socket, ip) = listener.accept().await?;

        let session = session.clone();
        let codec = EventCodec;
        let connection = Box::pin(codec.framed(socket));
        session.add_user(ip, connection);
    }
}
