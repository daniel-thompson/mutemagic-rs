use hidapi::HidApi;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa_sys::{SPA_PARAM_Props, SPA_PROP_mute, SPA_TYPE_OBJECT_Props};
use log::*;
use pipewire::{prelude::*, Context, MainLoop};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

const RED: u8 = 0x01;
const GREEN: u8 = 0x02;
const BLUE: u8 = 0x04;
const STRONG: u8 = 0x00;
const WEAK: u8 = 0x10;
const _FAST_PULSE: u8 = 0x20;
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
            (State::Confused, _) => RED | GREEN | SLOW_PULSE,
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
    mute: bool,
    node: pipewire::node::Node,
    _node_listener: pipewire::node::NodeListener,
}

impl StreamContext {
    fn new(node: pipewire::node::Node, node_listener: pipewire::node::NodeListener) -> Self {
        Self {
            mute: false,
            node,
            _node_listener: node_listener,
        }
    }
}

// TODO: impl Debug for StreamContext

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();

    let (tx, rx) = mpsc::channel::<Message>();
    let (pw_tx, pw_rx) = pipewire::channel::channel();

    {
        let tx = tx.clone();
        thread::spawn(move || {
            let api = HidApi::new().expect("Failed to create API instance");
            let puck = api.open(0x20a0, 0x42da).expect("Failed to open device");
            loop {
                let mut buf = [0u8; 8];
                let _res = puck.read(&mut buf[..]).unwrap();

                match buf[3] {
                    4 => {
                        let _ = tx.send(Message::Event(Event::Press));
                    }
                    2 => {
                        let _ = tx.send(Message::Event(Event::Release));
                    }
                    1 => { /* still pressed (and pointless) */ }
                    0 => { /* second "releases" message */ }
                    _ => (),
                }
            }
        });
    }

    {
        thread::spawn(move || {
            let api = HidApi::new().expect("Failed to create API instance");
            let puck = api.open(0x20a0, 0x42da).expect("Failed to open device");

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
                                State::Silent => State::Silent,
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
                let n2 = nodes.clone();
                let mut nodes = nodes.lock().unwrap();

                trace!("Scanning new global: {:?}", global);

                if let Some(properties) = &global.props {
                    if let Some("Stream/Input/Audio") = properties.get("media.class") {
                        if let Some("Manager") = properties.get("media.category") {
                            // Some of the nodes created by pavucontrol have media.class but
                            // can be distinguished from "real" input streams with this additional
                            // tag.
                            //
                            // Ignore anything with that tag!
                        } else {
                            if let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) {
                                let global_id = global.id;

                                trace!("Listening to node: {:?}", &node);

                                let node_listener = node
                                    .add_listener_local()
                                    .param({
                                        let tx = tx.clone();
                                        move |_s, _pt, _u1, _u2, _buf| {
                                            let mut nodes = n2.lock().unwrap();
                                            let (_, data) =
                                                PodDeserializer::deserialize_from::<Value>(&_buf)
                                                    .unwrap();

                                            trace!("Evaluating node parameter: {:?}", &data);

                                            if let Value::Object(data) = data {
                                                let v = data.properties.binary_search_by(|prop| {
                                                    prop.key.cmp(&SPA_PROP_mute)
                                                });
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

                                                    debug!(
                                                        "Mute change event: global id {} is {}",
                                                        global_id,
                                                        if mute { "muted" } else { "unmuted" }
                                                    );
                                                    let mut node = nodes.get_mut(&global_id);
                                                    if let Some(node) = &mut node {
                                                        node.mute = mute;

                                                        // TODO: implement the voting version...
                                                        let _ = tx.send(if mute {
                                                            Message::State(State::Muted)
                                                        } else {
                                                            Message::State(State::Unmuted)
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    })
                                    .register();

                                node.subscribe_params(&[libspa::param::ParamType::Props]);
                                //node.enum_params(0, Some(libspa::param::ParamType::Props), 0, u32::MAX);

                                nodes.insert(global.id, StreamContext::new(node, node_listener));
                                //trace!("Node list: {:?}", nodes);
                            }
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
                    // TODO: implement the voting version...
                    //
                    debug!("Removed global: {:?}", id);
                    let _ = tx.send(Message::State(State::Silent));
                }
                //trace!("Node list: {:?}", nodes);
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
