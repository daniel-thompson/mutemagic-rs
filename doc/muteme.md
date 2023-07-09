MuteMe USB HID protocol
=======================

This is an unofficial and (relatively) informal description of the MuteMe
HID protocol. It is the result of direct trial and error exploration of what
the MuteMe sends to its host and how it reacts to host packets.

Input reports
-------------

MuteMe provides an 8 byte input report. All information about the button
status is held in byte 3 (counting from zero):

 * #4: Button has transitioned from released to pressed
 * #2: Button has transitioned from pressed to released
 * #1: Button is still pressed (e.g. hardware auto-repeat). Can be
       ignored since #4 without a subsequent #2 indicates the button is
       still pressed.
 * #0: Button is not pressed. Can be ignored since it duplicates 2 (above)

Output reports
--------------

MuteMe provides an 8 byte input report. All information about the
requested activity is contained in byte 0.

* bit0: red LED
* bit1: green LED
* bit2: blue LED
* bit4 & 5: Mode
  - 00: High brightness
  - 01: Low brightness
  - 10: Fast pulse mode
  - 11: Slow pulse mode
* bit6: Enable timer (turn LED off after ~10s)

Notes:

 * Setting the lower 4-bits to 0x9 (bits 3 and 0) causes the device to reboot
   into bootloader mode
 * The timer bit is sticky. In other words the timer is armed when bit6
   transitions to set and cannot be reset until bit6 is lowered.
