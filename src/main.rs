use std::collections::HashSet;
use std::fs;
use std::panic::resume_unwind;
use std::ptr::write;
use libp2p::{
    identity, noise, PeerId, tcp, yamux, Transport,
    floodsub::{Topic}, mdns,
    swarm::{NetworkBehaviour, Swarm, SwarmBuilder}
};
use libp2p::core::upgrade::Version;
use libp2p::floodsub::Floodsub;
use libp2p::futures::AsyncBufReadExt;
use log::{error, info};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const STORAGE_FILE_PATH: &str = "./recipes.json";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;
type Recipes = Vec<Recipe>;


static KEYS: Lazy<identity::Keypair> = Lazy::new(|| identity::Keypair::generate_ed25519());
/// a unique ID for a specific peer
static PEER_ID: Lazy<PeerId> = Lazy::new(|| PeerId::from(KEYS.public()));
/// a Topic is something we can subscribe to and send messages on
static TOPIC: Lazy<Topic> = Lazy::new(|| Topic::new("recipes"));

#[derive(Debug, Serialize, Deserialize)]
struct Recipe {
    id: usize,
    name: String,
    ingredients: String,
    instructions: String,
    public: bool,
}

// Some messages we plan to send around

/// two ways to fetch lists from other peers: from all or from one
#[derive(Debug, Serialize, Deserialize)]
enum ListMode {
    All,
    One(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct ListRequest {
    mode: ListMode,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    mode: ListMode,
    data: Recipes,
    receiver: String,
}

/// distinguishes bw a response from another peer and input from ourselves
enum EventType {
    Response(ListResponse),
    Input(String),
}

/// defines the logic of the network and all peers.
/// mDNS is a protocol for discovering other peers on the local network
#[derive(NetworkBehaviour)]
struct RecipeBehaviour {
    floodsub: Floodsub,
    mdns: mdns::async_io::Behaviour,
    #[behaviour(ignore)]
    response_sender: mpsc::UnboundedSender<ListResponse>,
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    info!("Peer Id: {}", PEER_ID.clone());
    let (response_sender, mut response_rcv) = mpsc::unbounded_channel::<ListResponse>();

    /// Transport is a set of protocols that enables connection-oriented communication between peers.
    let transport = tcp::async_io::Transport::default()
        .upgrade(Version::V1)
        .authenticate(noise::Config::new(&KEYS).unwrap())
        .multiplex(yamux::Config::default())
        .boxed();

    let mut behaviour = RecipeBehaviour {
        floodsub: Floodsub::new(PEER_ID.clone()),
        mdns: mdns::async_io::Behaviour::new(
            mdns::Config::default(), PEER_ID.clone(),
        ).unwrap(),
        response_sender,
    };

    behaviour.floodsub.subscribe(TOPIC.clone());

    /// A swarm manages the connections created using the transport and executes the network behaviour we created,
    /// triggering and receiving events and giving us a way to get to them from the outside
    let mut swarm = SwarmBuilder::with_tokio_executor(
        transport, behaviour, PEER_ID.clone(),
    ).build();

    // defining async reader on STDIN, reading the stream line by line.
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    /// start our swarm, letting the OS decide the port for us
    Swarm::listen_on(
        &mut swarm,
        "/ip4/0.0.0.0/tcp/0"
            .parse()
            .expect("can get a local socket"),
    ).expect("swarm can be started");

    // defining an event loop, which listens to events from STDIN, from the Swarm, and from our response channel defined above
    loop {
        let event = {
            tokio::select! {
                line = stdin.next_line() => Some(EventType::Input(line.expect("can get line").expect("can read line from stdin"))),
                event = swarm.next() => {
                    info!("Unbalanced Swarm Event: {:?}", event);
                    None
                },
                response = response_rcv.recv() => Some(EventType::Response(response.expect("response exists"))),
            }
        };

        if let Some(evt) = event {
            match evt {
                EventType::Response(resp) => {
                    let json = serde_json::to_string(&resp).expect("can jsonify response");
                    swarm
                        .behaviour_mut()
                        .floodsub
                        .publish(TOPIC.clone(), json.as_bytes());
                }
                EventType::Input(line) => match line.as_str() {
                    "ls p" => handle_list_peers(&mut swarm).await,
                    cmd if cmd.starts_with("ls r") => handle_list_recipes(cmd, &mut swarm).await,
                    cmd if cmd.starts_with("create r") => handle_create_recipe(cmd).await,
                    cmd if cmd.starts_with("publish r") => handle_publish_recipe(cmd).await,
                    _ => error!("unknown command")
                }
            }
        }
    }
}

async fn create_new_recipe(name: &str, ingredients: &str, instructions: &str) -> Result<()> {
    let mut local_recipes = read_local_recipes().await?;
    let new_id = match local_recipes.iter().max_by_key(|r| r.id) {
        Some(v) => v.id + 1,
        None => 0
    };
    local_recipes.push( Recipe {
        id: new_id,
        name: name.to_owned(),
        ingredients: ingredients.to_owned(),
        instructions: instructions.to_owned(),
        public: false,
    });
    write_local_recipes(&local_recipes).await?;

    info!("Created recipe: ");
    info!("Name: {}", name);
    info!("Ingredients: {}", ingredients);
    info!("Instructions: {}", instructions);

    Ok(())
}

async fn publish_recipe(id: usize) -> Result<()> {
    let mut local_recipes = read_local_recipes().await?;
    local_recipes
        .iter_mut()
        .filter(|r| r.id == id)
        .for_each(|r| r.public = true);
    write_local_recipes(&local_recipes).await?;
    Ok(())
}

async fn read_local_recipes() -> Result<Recipes> {
    let content = fs::read(STORAGE_FILE_PATH).await?;
    let result = serde_json::from_slice(&content)?;
    Ok(result)
}

async fn write_local_recipes(recipes: &Recipes) -> Result<()> {
    let json = serde_json::to_string(&recipes)?;
    fs::write(STORAGE_FILE_PATH, &json).await?;
    Ok(())
}
/// lists all known peers
async fn handle_list_peers(swarm: &mut Swarm<RecipeBehaviour>) {
    info!("Discovered Peers: ");
    let nodes = swarm.behaviour().mdns.discovered_nodes();
    let mut unique_peers = HashSet::new();
    for peer in nodes {
        unique_peers.insert(peer)
    }
    unique_peers.iter().for_each(|p| info!("{}", p));
}

/// lists local recipes
async fn handle_list_recipes(cmd: &str, swarm: &mut Swarm<RecipeBehaviour>) {
    let rest = cmd.strip_prefix("ls r ");

    match rest {
        Some("all") => {
            let req = ListRequest {
                mode: ListMode::All
            };

            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .floodsub
                .publish(TOPIC.clone(), json.as_bytes());
        }
        Some(recipes_peer_id) => {
            let req = ListRequest {
                mode: ListMode::One(recipes_peer_id.to_owned())
            };

            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .floodsub
                .publish(TOPIC.clone(), json.as_bytes());
        }
        None => {
            match read_local_recipes().await {
                Ok(v) => {
                    info!("Local Recipes ({})", v.len());
                    v.iter().for_each(|r| info!("{:?}", r));
                }
                Err(e) => error!("error fetching local recipes: {}", e),
            };
        }
    };
}

/// creates a new recipe
async fn handle_create_recipe(cmd: &str) {
    if let Some(rest) = cmd.strip_prefix("create r") {
        let elements: Vec<&str> = rest.split("|").collect();
        if elements.len() < 3 {
            info!("too few arguments - Format: name|ingredients|instructions");
        }
        let name = elements.get(0).expect("name is there");
        let ingredients = elements.get(1).expect("ingredients are there");
        let instructions = elements.get(2).expect("instructions are there");
        if let Err(e) = create_new_recipe(name, ingredients, instructions).await {
            error!("error creating recipe: {}", e);
        };
    }
}

/// publishes a recipe
async fn handle_publish_recipe(cmd: &str) {
    if let Some(rest) = cmd.strip_prefix("publish r") {
        match rest.trim().parse::<usize>() {
            Ok(id) => {
                if let Err(e) = publish_recipe(id).await {
                    info!("error publishing recipe with id {}, {}", id, e)
                }
                info!("Published Recipe with id: {}", id);
            }
            Err(e) => error!("invalid id: {}, {}", rest.trim(), e),
        };
    }
}