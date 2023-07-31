mutemagic-rs - Linux userspace driver for USB HID mute buttons
==============================================================

MuteMagic is an open-source userspace daemon that monitors pipewire
streams and uses that information to manage mute buttons.

Currently MuteMagic provides drivers for the MuteMe Original (although
test reports for the MuteMe Mini would be very welcome).  Extending
mutemagic-rs so other similar products (or DIY button pads) should be
very simple: there is far more code to manage pipewire than there is to
manipulate the hardware!

Features
--------

 * Indicates when the microphones are active and inactive
 * Preserves mute state through application relaunch (if supported by
   the pipewire session manager)
 * Power efficient. MuteMagic does not use polling to track the button
   state and therefore manages zero wake ups per second whilst the
   device is plugged in. Note that basic integration using the user-level
   systemd service does not receive hotplug events so when the device is
   not connected there will a hotplug scan every five seconds.

How it works
------------

MuteMagic monitors pipewire for capture streams. When a stream is
created MuteMagic gets the current mute status and displays that using
LEDs in the mute button.

By monitoring for active capture streams MuteMagic can show more than
just the mute state! It is also able to show whether the microphone is
in use or not. This works for applications that use pipewire,
pulseaudio and jack libraries to communicate with pipewire. It also
works for raw ALSA applications, providing those applications use the
default ALSA configuration (rather than going direct to the hardware
devices).

Additionally by muting capture streams rather then hardware sources,
MuteMagic allows the Pipewire session manager to maintain a different
mute states for different applications (e.g. for each application
pipewire will remember the most recently used mute state and MuteMagic
will show the correct mute state).

When using a MuteMe Original the different states are presented as shown
below:

 * Off -> No audio capture streams are running
 * Green -> Unmuted, meaning that one or more audio capture streams are
   currently running and every stream is unmuted.
 * Pulsing red -> Muted, meaning that one more audio capture streams are
   currently running and every stream is muted.
 * Pulsing green -> Partially unmuted, meaning two or more audio capture
   streams are running, some of which are muted and some of which are
   unmuted.

Usage
-----

To run in place try:

~~~sh
RUST_LOG=info cargo run
~~~~

Set the log level to whatever is appropriate. `debug` shows basic
activity whilst `debug` and `trace` will show, at different levels of
detail, the data structures received from pipewire and what MuteMagic
does with them. The debug mode logs are especially useful when debugging
and extending the pipewire integration.

To install try:

~~~sh
git clone https://github.com/daniel-thompson/mutemagic-rs
cd mutemagic-rs
cargo install --path .
mutemagic-rs
~~~

License
-------

This program is free software: you can redistribute it and/or modify it
under the terms of the GNU General Public License as published by the
Free Software Foundation, either version 3 of the License, or (at your
option) any later version.

This program is distributed in the hope that it will be useful, but
**without any warranty**; without even the implied warranty of
**merchantability** or **fitness for a particular purpose**.  See the
[GNU General Public License](LICENSE.md) for more details.
