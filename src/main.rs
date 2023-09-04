// SPDX-License-Identifier: GPL-3.0-or-later

use hidapi::HidApi;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa_sys::{SPA_PARAM_Props, SPA_PROP_mute, SPA_TYPE_OBJECT_Props};
use log::*;
use pipewire::{prelude::*, Context, MainLoop};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::fd::AsRawFd;
use std::rc::Rc;
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
    Hotplug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Silent,
    Muted,
    Unmuted,
    Confused,
}

impl State {
    #[rustfmt::skip] // don't reformat the "ternary" conditions
    fn from_bools(nodes: impl Iterator<Item=bool>) -> State {
        nodes.fold(State::Silent, |state, mute| match state {
            State::Silent => if mute { State::Muted } else { State::Unmuted },
            State::Muted => if mute { State::Muted } else { State::Confused },
            State::Unmuted => if mute { State::Confused } else { State::Unmuted },
            State::Confused => State::Confused,
        })
    }

    fn msg(&self, event: &Event) -> u8 {
        match (self, event) {
            (State::Silent, Event::Press) => BLUE | WEAK,
            (State::Silent, _) => 0,
            (State::Muted, Event::Press) => RED | STRONG,
            (State::Muted, _) => RED | SLOW_PULSE,
            (State::Unmuted, Event::Press) => GREEN | STRONG,
            (State::Unmuted, _) => GREEN | WEAK,
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
    mute: bool,
    node: pipewire::node::Node,
    _node_listener: pipewire::node::NodeListener,
}

impl StreamContext {
    fn new(node: pipewire::node::Node, update_mute: impl Fn(bool) + 'static) -> Self {
        debug!("Listening to node: {:?}", &node);

        // This listener is not used after creation (and has no useful methods)
        // hence the underscore naming. However it *must* be moved into the
        // stream context to ensure it is not dropped.
        let _node_listener = node
            .add_listener_local()
            .param(move |_s, _pt, _u1, _u2, buf| {
                let (_, data) = PodDeserializer::deserialize_from::<Value>(&buf).unwrap();

                debug!("Evaluating node parameter: {:?}", &data);

                if let Value::Object(data) = data {
                    let i = data
                        .properties
                        .binary_search_by(|prop| prop.key.cmp(&SPA_PROP_mute));
                    if let Ok(i) = i {
                        if let Value::Bool(mute) = data.properties[i].value {
                            update_mute(mute);
                        }
                    }
                }
            })
            .register();

        // There is no need to call node.enum_params() here because subscribing
        // is enough to get the above closure called to collect the current state
        // of the parameters.
        node.subscribe_params(&[libspa::param::ParamType::Props]);

        Self {
            mute: false,
            node,
            _node_listener,
        }
    }
}

impl fmt::Debug for StreamContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Point")
            .field("mute", &self.mute)
            .field("node", &self.node)
            .finish()
    }
}

struct HotplugMonitor {
    socket: udev::MonitorSocket,
}

impl HotplugMonitor {
    fn new(subsys: &str) -> io::Result<Self> {
        let socket = udev::MonitorBuilder::new()?
            .match_subsystem(subsys)?
            .listen()?;

        Ok(Self { socket })
    }

    fn wait_for_event(&self) -> io::Result<udev::Event> {
        let mut fds = [nix::poll::PollFd::new(
            self.socket.as_raw_fd(),
            nix::poll::PollFlags::POLLIN,
        )];

        let result = nix::poll::poll(&mut fds, -1);
        if let Err(_errno) = result {
            Err(io::Error::last_os_error())
        } else {
            match self.socket.iter().next() {
                Some(evt) => Ok(evt),
                None => unreachable!(),
            }
        }
    }

    fn clear_events(&self) {
        for _ in self.socket.iter() {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler, then exit the process
        orig_hook(panic_info);

        std::process::exit(1);
    }));

    let (tx, rx) = pipewire::channel::channel::<Message>();

    {
        let tx = tx.clone();

        thread::spawn(move || {
            let monitor = HotplugMonitor::new("hidraw").expect("Cannot monitor hotplug events");

            loop {
                let api = HidApi::new().expect("Failed to create API instance");
                let puck = api.open(0x20a0, 0x42da);

                if let Ok(puck) = puck {
                    info!("Connected to mute device");
                    monitor.clear_events();
                    let _ = tx.send(Message::Event(Event::Hotplug));

                    loop {
                        let mut buf = [0u8; 8];
                        let res = puck.read(&mut buf[..]);
                        if let Err(_) = res {
                            break;
                        }

                        match buf[3] {
                            4 => {
                                let _ = tx.send(Message::Event(Event::Press));
                            }
                            0 => {
                                let _ = tx.send(Message::Event(Event::Release));
                            }
                            _ => (),
                        }
                    }
                } else {
                    info!("Waiting for add event");
                    while {
                        let event = monitor.wait_for_event();
                        if let Ok(event) = event {
                            event.event_type() != udev::EventType::Add
                        } else {
                            info!("Re-waiting for event");
                            true
                        }
                    } {}
                }
            }
        });
    }

    //
    // At this point we've kicked off the thread to manage input from the
    // puck so we can use the main thread to run the pipewire main loop.
    //

    let mainloop = MainLoop::new()?;
    let context = Context::new(&mainloop)?;
    let core = context.connect(None)?;
    let registry = core.get_registry()?;

    let nodes = Rc::new(RefCell::new(HashMap::<u32, StreamContext>::new()));

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
                            let ctx = StreamContext::new(node, {
                                let global_id = global.id;
                                let nodes = nodes.clone();
                                let tx = tx.clone();

                                move |mute| {
                                    info!(
                                        "Mute change event: global id {} is {}",
                                        global_id,
                                        if mute { "muted" } else { "unmuted" }
                                    );

                                    let mut nodes = nodes.borrow_mut();
                                    let mut node = nodes.get_mut(&global_id);
                                    if let Some(node) = &mut node {
                                        node.mute = mute;

                                        let state = State::from_bools(
                                            nodes.iter().map(|(_, ctx)| ctx.mute),
                                        );
                                        let _ = tx.send(Message::State(state));
                                    }
                                }
                            });

                            nodes.borrow_mut().insert(global.id, ctx);
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
                let mut nodes = nodes.borrow_mut();
                let node = nodes.remove(&id);
                if let Some(_node) = node {
                    info!("Removed global: {:?}", id);
                    let state = State::from_bools(nodes.iter().map(|(_, ctx)| ctx.mute));
                    let _ = tx.send(Message::State(state));
                }
                trace!("Node list: {:?}", nodes);
            }
        })
        .register();

    let _rx = rx.attach(&mainloop, {
        let mute_state = Cell::new(State::Silent);
        let button_state = Cell::new(Event::Release);

        move |msg| {
            match msg {
                Message::State(state) => {
                    mute_state.set(state);
                }
                Message::Event(evt) => {
                    button_state.set(evt);

                    // On button release we update the current mute state and,
                    // if needed, ask the pipewire thread to update the streams
                    if let Event::Release = button_state.get() {
                        let next_mute_state = match mute_state.get() {
                            State::Silent => State::Silent,
                            State::Muted => State::Unmuted,
                            _ => State::Muted,
                        };

                        if let Some(mute) = next_mute_state.is_muted() {
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

                                let nodes = nodes.borrow_mut();

                                for (_k, ctx) in nodes.iter() {
                                    ctx.node.set_param(libspa::param::ParamType::Props, 0, data);
                                }
                            } else {
                                unreachable!();
                            }
                        } else if State::Silent == next_mute_state {
                            info!("No mute change event (no streams)");
                        }
                    }
                }
            }

            let api = HidApi::new().expect("Failed to create API instance");
            let puck = api.open(0x20a0, 0x42da);
            if let Ok(puck) = puck {
                let _ = puck.write(&[mute_state.get().msg(&button_state.get()), 0, 0]);
            }
        }
    });

    ctrlc::set_handler(move || {
        // Try to clear the LEDs before we exit...
        let api = HidApi::new().expect("Failed to create API instance");
        let puck = api.open(0x20a0, 0x42da);
        if let Ok(puck) = puck {
            let _ = puck.write(&[0, 0, 0]);
        }
        std::process::exit(1);
    })
    .unwrap();

    mainloop.run();

    Ok(())
}
