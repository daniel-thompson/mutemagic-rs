// SPDX-License-Identifier: GPL-3.0-or-later

use hidapi::HidApi;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa_sys::{SPA_PARAM_Props, SPA_PROP_mute, SPA_TYPE_OBJECT_Props};
use log::*;
use pipewire::{prelude::*, Context, MainLoop};
use std::collections::HashMap;
use std::fmt;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

const RED: u8 = 0x01;
const GREEN: u8 = 0x02;
const BLUE: u8 = 0x04;
const STRONG: u8 = 0x00;
const WEAK: u8 = 0x10;
const FAST_PULSE: u8 = 0x20;
const SLOW_PULSE: u8 = 0x30;

#[derive(Debug, Clone, Copy)]
enum Event {
    Press,
    Release,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Silent,
    Muted,
    Unmuted,
    Confused,
}

impl State {
    fn msg(&self, event: &Event) -> u8 {
        match (self, event) {
            (State::Silent, Event::Press) => BLUE | WEAK,
            (State::Silent, Event::Release) => 0,
            (State::Muted, Event::Press) => RED | STRONG,
            (State::Muted, Event::Release) => RED | SLOW_PULSE,
            (State::Unmuted, Event::Press) => GREEN | STRONG,
            (State::Unmuted, Event::Release) => GREEN | WEAK,
            (State::Confused, _) => RED | GREEN | FAST_PULSE,
        }
    }

    fn is_muted(&self) -> Option<bool> {
        match self {
            State::Muted => Some(true),
            State::Unmuted => Some(false),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Message {
    State(State),
    Event(Event),
}

struct StreamContext {
    global_id: u32,
    mute: bool,
    node: pipewire::node::Node,
    _node_listener: pipewire::node::NodeListener,
}

fn get_state_from_nodes(nodes: &HashMap<u32, StreamContext>) -> State {
    nodes
        .iter()
        .fold(State::Silent, |state, (_, ctx)| match state {
            State::Silent => {
                if ctx.mute {
                    State::Muted
                } else {
                    State::Unmuted
                }
            }
            State::Muted => {
                if ctx.mute {
                    State::Muted
                } else {
                    State::Confused
                }
            }
            State::Unmuted => {
                if ctx.mute {
                    State::Confused
                } else {
                    State::Unmuted
                }
            }
            State::Confused => State::Confused,
        })
}

impl StreamContext {
    fn new(
        global_id: u32,
        node: pipewire::node::Node,
        tx: mpsc::Sender<Message>,
        nodes: Arc<Mutex<HashMap<u32, StreamContext>>>,
    ) -> Self {
        debug!("Listening to node: {:?}", &node);

        let node_listener = node
            .add_listener_local()
            .param(move |_s, _pt, _u1, _u2, buf| {
                let (_, data) = PodDeserializer::deserialize_from::<Value>(&buf).unwrap();

                debug!("Evaluating node parameter: {:?}", &data);

                if let Value::Object(data) = data {
                    let v = data
                        .properties
                        .binary_search_by(|prop| prop.key.cmp(&SPA_PROP_mute));
                    if let Ok(i) = v {
                        let mute = &data.properties[i];

                        // At this state we have found the mute property. We
                        // know that should be a bool to we don't mind panicing
                        // if it isn't
                        let mute = if let Value::Bool(m) = mute.value {
                            m
                        } else {
                            unreachable!();
                        };

                        info!(
                            "Mute change event: global id {} is {}",
                            global_id,
                            if mute { "muted" } else { "unmuted" }
                        );

                        let mut nodes = nodes.lock().unwrap();
                        let mut node = nodes.get_mut(&global_id);
                        if let Some(node) = &mut node {
                            node.mute = mute;
                            let _ = tx.send(Message::State(get_state_from_nodes(&nodes)));
                        }
                    }
                }
            })
            .register();

        node.subscribe_params(&[libspa::param::ParamType::Props]);
        //node.enum_params(0, Some(libspa::param::ParamType::Props), 0, u32::MAX);

        Self {
            global_id,
            mute: false,
            node,
            _node_listener: node_listener,
        }
    }
}

impl fmt::Debug for StreamContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Point")
            .field("global_id", &self.global_id)
            .field("mute", &self.mute)
            .field("node", &self.node)
            .finish()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler and exit the process
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    let (tx, rx) = mpsc::channel::<Message>();
    let (pw_tx, pw_rx) = pipewire::channel::channel();

    let api = HidApi::new().expect("Failed to create API instance");

    {
        let puck = api.open(0x20a0, 0x42da).expect("Failed to open device");
        let tx = tx.clone();

        thread::spawn(move || loop {
            let mut buf = [0u8; 8];
            let _res = puck.read(&mut buf[..]).unwrap();

            match buf[3] {
                4 => {
                    let _ = tx.send(Message::Event(Event::Press));
                }
                2 => {
                    let _ = tx.send(Message::Event(Event::Release));
                }
                _ => (),
            }
        });
    }

    {
        let puck = api.open(0x20a0, 0x42da).expect("Failed to open device");

        thread::spawn(move || {
            let mut led = [0u8, 0, 0];

            // Set a default initial state and update the puck accordingly
            let mut mute_state = State::Silent;
            let mut button_state = Event::Release;
            let _ = puck.write(&led);

            loop {
                let msg = rx.recv().unwrap_or(Message::State(State::Confused));

                match msg {
                    Message::Event(evt) => {
                        button_state = evt;

                        // On button release we update the current mute state
                        // and ask the pipewire thread to update the streams
                        if let Event::Release = button_state {
                            let new_state = match mute_state {
                                State::Silent => {
                                    info!("No mute change event (no streams)");
                                    State::Silent
                                }
                                State::Muted => State::Unmuted,
                                _ => State::Muted,
                            };

                            let _ = pw_tx.send(new_state);
                        }
                    }
                    Message::State(state) => {
                        mute_state = state;
                    }
                }

                led[0] = mute_state.msg(&button_state);
                let _ = puck.write(&led);
            }
        });
    }

    //
    // At this point we've kicked off the two threads to manage the puck so we
    // can leave this thread to run the pipewire main loop.
    //

    let mainloop = MainLoop::new()?;
    let context = Context::new(&mainloop)?;
    let core = context.connect(None)?;
    let registry = core.get_registry()?;

    let nodes = Arc::new(Mutex::new(HashMap::<u32, StreamContext>::new()));

    // Register a callback to the `global` event on the registry, which notifies of any new global objects
    // appearing on the remote.
    // The callback will only get called as long as we keep the returned listener alive.
    let _listener = registry
        .add_listener_local()
        .global({
            let nodes = nodes.clone();
            let tx = tx.clone();
            let registry = core.get_registry()?;

            move |global| {
                debug!("Scanning new global: {:?}", global);

                if let Some(properties) = &global.props {
                    let media_class = properties.get("media.class").unwrap_or("");
                    let media_category = properties.get("media.category").unwrap_or("");
                    let application_name = properties.get("application.name").unwrap_or("UNKNOWN");

                    // Programs like GNOME Settings and PulseAudio Volume Control create nodes
                    // where media.class is "Stream/Input/Audio".
                    //
                    // In newer distros we can recognise these by looking at the media.category field.
                    // Sadly in older distros this does not work and we simply rely on an ignore
                    // list. Ideas a better heuristic would be very welcome...
                    const IGNORE_LIST: &[&str] = &["GNOME Settings", "PulseAudio Volume Control"];
                    let ignore =
                        media_category == "Manager" || IGNORE_LIST.contains(&application_name);

                    if media_class == "Stream/Input/Audio" && !ignore {
                        if let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) {
                            let ctx =
                                StreamContext::new(global.id, node, tx.clone(), nodes.clone());

                            let mut nodes = nodes.lock().unwrap();
                            nodes.insert(global.id, ctx);
                            info!("Added global: {:?} ({})", global.id, application_name);
                            trace!("Node list: {:?}", nodes);
                        }
                    }
                }
            }
        })
        .global_remove({
            let nodes = nodes.clone();
            let tx = tx.clone();

            move |id| {
                let mut nodes = nodes.lock().unwrap();
                let node = nodes.remove(&id);
                if let Some(_node) = node {
                    info!("Removed global: {:?}", id);
                    let _ = tx.send(Message::State(get_state_from_nodes(&nodes)));
                }
                trace!("Node list: {:?}", nodes);
            }
        })
        .register();

    let _rx = pw_rx.attach(&mainloop, move |state| {
        if let Some(mute) = state.is_muted() {
            let pod = Value::Object(Object {
                type_: SPA_TYPE_OBJECT_Props,
                id: SPA_PARAM_Props,
                properties: vec![Property {
                    key: SPA_PROP_mute,
                    flags: PropertyFlags::empty(),
                    value: Value::Bool(mute),
                }],
            });

            let mut data = std::io::Cursor::new(Vec::<u8>::new());
            let res = PodSerializer::serialize(&mut data, &pod);

            if let Ok((data, len)) = res {
                let data = data.get_ref();
                assert!(data.len() == len as usize);

                let nodes = nodes.lock().unwrap();

                for (_k, ctx) in nodes.iter() {
                    ctx.node.set_param(libspa::param::ParamType::Props, 0, data);
                }
            } else {
                unreachable!();
            }
        }
    });

    mainloop.run();

    Ok(())
}
