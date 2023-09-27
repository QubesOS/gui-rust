/*
 * The Qubes OS Project, http://www.qubes-os.org
 *
 * Copyright (C) 2010  Rafal Wojtczuk  <rafal@invisiblethingslab.com>
 * Copyright (C) 2021  Demi Marie Obenour  <demi@invisiblethingslab.com>
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
 *
 */

//! # Rust bindings to, and specification of, the Qubes OS GUI Protocol (QOGP).
//!
//! ## Transport and Terminology
//!
//! The Qubes OS GUI protocol is spoken over a vchan between two virtual
//! machines (VMs).  The VM providing GUI services is the client of this vchan,
//! while the VM that wishes to display its GUI is the server.  The component
//! that provides GUI services to other VMs is known as the *GUI daemon*, and
//! the component that the GUI daemon connects to is known as the *GUI agent*.
//!
//! ## Message format
//!
//! Each message is a C struct that is cast to a byte slice and sent
//! directly over the vchan, without any marshalling or unmarshalling steps.
//! This is safe because no GUI message has any padding bytes.  Similarly, the
//! receiver casts a C struct to a mutable byte slice and reads the bytes
//! directly into the struct.  This is safe because all possible bit patterns
//! are valid for every GUI message.  All messages are in native byte order,
//! which is little-endian for the only platform (amd64) supported by Qubes OS.
//!
//! This is very natural to implement in C, but is much less natural to
//! implement in Rust, as casting a struct reference to a byte slice is
//! `unsafe`.  To ensure that this does not cause security vulnerabilities,
//! this library uses the `qubes-castable` crate.  That crate provides a
//! `castable!` macro to define structs that can be safely casted to a byte
//! slice.  `castable!` guarantees that every struct it defines can be safely
//! cast to a byte slice and back; if it cannot, a compile-time error results.
//! Functions provided by the `qubes-castable` crate are used to perform the
//! conversions.  To ensure that they cannot be called on inappropriate types
//! (such as `bool`), they require the unsafe `Castable` trait to be implemented.
//! The `castable!` macro implements this trait for every type it defines, and
//! the `qubes-castable` crate implements it for all fixed-width primitive
//! integer types, `()`, and arrays of `Castable` objects (regardless of length).
//!
//! Both clients and servers MUST send each message atomically.  Specifically,
//! the server MAY use blocking I/O over the vchan.  The client MUST NOT block
//! on the server, to avoid deadlocks.  Therefore, the client should buffer its
//! messages and flush them at every opportunity.  This requirement is a
//! consequence of how difficult asynchronous I/O is in C, and of the desire to
//! keep the code as simple as possible.  Implementations in other languages, or
//! which uses proper asynchronous I/O libraries, SHOULD NOT have this
//! limitation.
//!
//! ## Window IDs
//!
//! The Qubes OS GUI protocol refers to each surface by a 32-bit unsigned window
//! ID.  Zero is reserved and means “no window”.  For instance, using zero for a
//! window’s parent means that the window does not have a parent.  Otherwise,
//! agents are free to choose any window ID they wish.  In particular, while X11
//! limits IDs to a maximum of 2²⁹ - 1, the Qubes OS GUI protocol imposes no
//! such restriction.
//!
//! It is a protocol error for an agent to send a message to a window that does
//! not exist, including a window which it has deleted.  It is also a protocol
//! error for an agent to try to create a window with an ID that is already in
//! use.  Because of unavoidable race conditions, agents may recieve events for
//! windows they have already destroyed.  Such messages MUST be ignored until
//! the daemon acknowledges the window’s destruction.  Agents must not
//! reuse a window ID until such an acknowledgement has been received.
//!
//! ## Unrecognized messages
//!
//! GUI daemons MUST treat messages with an unknown type as a protocol error.
//! GUI agents MAY log the headers of such messages and MUST otherwise ignore
//! them.  The bodies of such messages MUST NOT be logged as they may contain
//! sensitive data.
//!
//! ## Shared memory
//!
//! The Qubes GUI protocol uses inter-qube shared memory for all images.  This
//! shared memory is not sanitized in any way whatsoever, and may be modified
//! by the other side at any time without synchronization.  Therefore, all
//! access to the shared memory is `unsafe`.  Or rather, it *would* be unsafe,
//! were it not that no such access is required at all!  This avoids requiring
//! any form of signal handling, which is both `unsafe` and ugly.
//!
//! ## Differences from the reference implementation
//!
//! The reference implementation of the GUI protocol considers the GUI daemon
//! (the server) to be trusted, while the GUI agent is not trusted.  As such,
//! the GUI agent blindly trusts the GUI daemon, while the GUI daemon must
//! carefully validate all data from the GUI agent.
//!
//! This Rust implementation takes a different view: *Both* the client and server
//! consider the other to be untrusted, and all messages are strictly validated.
//! This is necessary to meet Rust safety requirements, and also makes bugs in
//! the server easier to detect.
//!
//! Additionally, the Rust protocol definition is far, *far* better documented,
//! and explicitly lists each reference to the X11 protocol specification.  A
//! future release will not depend on the X11 protocol specification at all,
//! even for documentation.

#![forbid(missing_docs)]
#![no_std]
#![forbid(clippy::all)]

use core::convert::TryFrom;
use core::num::NonZeroU32;
use core::result::Result;

/// Arbitrary maximum size of a clipboard message
pub const MAX_CLIPBOARD_SIZE: u32 = 65000;

/// Arbitrary max window height
pub const MAX_WINDOW_HEIGHT: u32 = 6144;

/// Arbitrary max window width
pub const MAX_WINDOW_WIDTH: u32 = 16384;

/// Default cursor ID.
pub const CURSOR_DEFAULT: u32 = 0;

/// Flag that must be set to request an X11 cursor
pub const CURSOR_X11: u32 = 0x100;

/// Max X11 cursor that can be requested
pub const CURSOR_X11_MAX: u32 = 0x19a;

/// Bits-per-pixel of the dummy X11 framebuffer driver
pub const DUMMY_DRV_FB_BPP: u32 = 32;

/// Maximum size of a shared memory segment, in bytes
pub const MAX_WINDOW_MEM: u32 = MAX_WINDOW_WIDTH * MAX_WINDOW_HEIGHT * (DUMMY_DRV_FB_BPP / 8);

/// Number of bytes in a shared page
pub const XC_PAGE_SIZE: u32 = 1 << 12;

/// Maximum permissable number of shared memory pages in a single segment using
/// deprecated privcmd-based shared memory
pub const MAX_MFN_COUNT: u32 = (MAX_WINDOW_MEM + XC_PAGE_SIZE - 1) >> 12;

/// Maximum permissable number of shared memory pages in a single segment using
/// grant tables
pub const MAX_GRANT_REFS_COUNT: u32 = (MAX_WINDOW_MEM + XC_PAGE_SIZE - 1) >> 12;

/// GUI agent listening port
pub const LISTENING_PORT: i16 = 6000;

/// Type of grant refs dump messages
pub const WINDOW_DUMP_TYPE_GRANT_REFS: u32 = 0;

/// The major version of the protocol
pub const PROTOCOL_VERSION_MAJOR: u32 = 1;

/// The minor version of the protocol.
pub const PROTOCOL_VERSION_MINOR: u32 = 7;

/// The overall protocol version, as used on the wire.
pub const PROTOCOL_VERSION: u32 = PROTOCOL_VERSION_MAJOR << 16 | PROTOCOL_VERSION_MINOR;

// This allows pattern-matching against constant values without a huge amount of
// boilerplate code.
macro_rules! enum_const {
    (
        #[repr($t: ty)]
        $(#[$i: meta])*
        $p: vis enum $n: ident {
            $(
                $(#[$j: meta])*
                ($const_name: ident, $variant_name: ident) $(= $e: expr)?
            ),*$(,)?
        }
    ) => {
        $(#[$i])*
        #[repr($t)]
        $p enum $n {
            $(
                $(#[$j])*
                $variant_name $(= $e)?,
            )*
        }

        $(
            $(#[$j])*
            $p const $const_name: $t = $n::$variant_name as $t;
        )*

        impl $crate::TryFrom::<$t> for $n {
            type Error = $t;
            #[allow(non_upper_case_globals)]
            #[inline]
            fn try_from(value: $t) -> $crate::Result<Self, $t> {
                match value {
                    $(
                        $const_name => return $crate::Result::Ok($n::$variant_name),
                    )*
                    other => $crate::Result::Err(other),
                }
            }
        }
    }
}

enum_const! {
    #[repr(u32)]
    #[non_exhaustive]
    /// Message types
    pub enum Msg {
        /// Daemon ⇒ agent: A key has been pressed or released.
        (MSG_KEYPRESS, Keypress) = 124,
        /// Daemon ⇒ agent: A button has been pressed or released.
        (MSG_BUTTON, Button),
        /// Daemon ⇒ agent: Pointer has moved.
        (MSG_MOTION, Motion),
        /// Daemon ⇒ agent: The pointer has entered or left a window.
        (MSG_CROSSING, Crossing),
        /// Daemon ⇒ agent: A window has just acquired focus.
        (MSG_FOCUS, Focus),
        /// Daemon ⇒ agent, obsolete.
        (MSG_RESIZE, Resize),
        /// Agent ⇒ daemon: Creates a window.
        (MSG_CREATE, Create),
        /// Agent ⇒ daemon: Destroys a window.
        (MSG_DESTROY, Destroy),
        /// Bidirectional: A part of the window must be redrawn.
        (MSG_MAP, Map),
        /// Agent ⇒ daemon: Unmap a window
        (MSG_UNMAP, Unmap) = 133,
        /// Bidirectional: A window has been moved and/or resized.
        (MSG_CONFIGURE, Configure),
        /// Ask dom0 (only!) to map the given amount of memory into composition
        /// buffer.  Deprecated.
        (MSG_MFNDUMP, MfnDump),
        /// Agent ⇒ daemon: Redraw given area of screen.
        (MSG_SHMIMAGE, ShmImage),
        /// Daemon ⇒ agent: Request that a window be destroyed.
        (MSG_CLOSE, Close),
        /// Daemon ⇒ agent, deprecated, DO NOT USE
        (MSG_EXECUTE, Execute),
        /// Daemon ⇒ agent: Request clipboard data.
        (MSG_CLIPBOARD_REQ, ClipboardReq),
        /// Bidirectional: Clipboard data
        (MSG_CLIPBOARD_DATA, ClipboardData),
        /// Agent ⇒ daemon: Set the title of a window.  Called MSG_WMNAME in C.
        (MSG_SET_TITLE, SetTitle),
        /// Daemon ⇒ agent: Update the keymap
        (MSG_KEYMAP_NOTIFY, KeymapNotify),
        /// Agent ⇒ daemon: Dock a window
        (MSG_DOCK, Dock) = 143,
        /// Agent ⇒ daemon: Set window manager hints.
        (MSG_WINDOW_HINTS, WindowHints),
        /// Bidirectional: Set window manager flags.
        (MSG_WINDOW_FLAGS, WindowFlags),
        /// Agent ⇒ daemon: Set window class.
        (MSG_WINDOW_CLASS, WindowClass),
        /// Agent ⇒ daemon: Send shared memory dump
        (MSG_WINDOW_DUMP, WindowDump),
        /// Agent ⇒ daemon: Set cursor type
        (MSG_CURSOR, Cursor),
        /// Daemon ⇒ agent: Acknowledge mapping (version 1.7+ only)
        (MSG_WINDOW_DUMP_ACK, DumpAck),
    }
}

enum_const! {
    #[repr(u32)]
    /// State of a button
    pub enum ButtonEvent {
        /// A button has been pressed
        (EV_BUTTON_PRESS, Press) = 4,
        /// A button has been released
        (EV_BUTTON_RELEASE, Release) = 5,
    }
}

enum_const! {
    #[repr(u32)]
    /// Key change event
    pub enum KeyEvent {
        /// The key was pressed
        (EV_KEY_PRESS, Press) = 2,
        /// The key was released
        (EV_KEY_RELEASE, Release) = 3,
    }
}

enum_const! {
    #[repr(u32)]
    /// Focus change event
    pub enum FocusEvent {
        /// The window now has focus
        (EV_FOCUS_IN, In) = 9,
        /// The window has lost focus
        (EV_FOCUS_OUT, Out) = 10,
    }
}

/// Flags for [`WindowHints`].  These are a bitmask.
pub enum WindowHintsFlags {
    /// User-specified position
    USPosition = 1 << 0,
    /// Program-specified position
    PPosition = 1 << 2,
    /// Minimum size is valid
    PMinSize = 1 << 4,
    /// Maximum size is valid
    PMaxSize = 1 << 5,
    /// Resize increment is valid
    PResizeInc = 1 << 6,
    /// Base size is valid
    PBaseSize = 1 << 8,
}

/// Flags for [`WindowFlags`].  These are a bitmask.
pub enum WindowFlag {
    /// Fullscreen request.  This may or may not be honored.
    Fullscreen = 1 << 0,
    /// Demands attention
    DemandsAttention = 1 << 1,
    /// Minimize
    Minimize = 1 << 2,
}

/// Trait for Qubes GUI structs, specifying the message number.
pub trait Message: qubes_castable::Castable + core::default::Default {
    /// The kind of the message
    const KIND: Msg;
}

impl From<NonZeroU32> for WindowID {
    fn from(other: NonZeroU32) -> Self {
        Self {
            window: Some(other),
        }
    }
}

impl From<u32> for WindowID {
    fn from(other: u32) -> Self {
        qubes_castable::cast!(other)
    }
}

qubes_castable::castable! {
    /// A window ID.
    pub struct WindowID {
        /// The window ID, or `None` for the special whole-screen window.  The
        /// whole-screen window always exists.  Trying to create it is a
        /// protocol error.
        pub window: Option<NonZeroU32>,
    }

    /// A GUI message as it appears on the wire.  All fields are in native byte
    /// order.
    pub struct UntrustedHeader {
        /// Type of the message
        pub ty: u32,
        /// Window to which the message is directed.
        ///
        /// For all messages *except* CREATE, the window MUST exist.  For CREATE,
        /// the window MUST NOT exist.
        pub window: WindowID,
        /// UNTRUSTED length value.  The GUI agent MAY use this to skip unknown
        /// message.  The GUI daemon MUST NOT use this to calculate the message
        /// length without sanitizing it first.
        pub untrusted_len: u32,
    }

    /// X and Y coordinates relative to the top-left of the screen
    pub struct Coordinates {
        /// X coordinate in pixels
        pub x: i32,
        /// Y coordinate in pixels
        pub y: i32,
    }

    /// Window size
    pub struct WindowSize {
        /// Width in pixels
        pub width: u32,
        /// Height in pixels
        pub height: u32,
    }

    /// A (x, y, width, height) tuple
    pub struct Rectangle {
        /// Coordinates of the top left corner of the rectangle
        pub top_left: Coordinates,
        /// Size of the rectangle
        pub size: WindowSize
    }

    /// Daemon ⇒ agent: Root window configuration; sent only at startup,
    /// without a header.  Only used in protocol 1.3 and below.
    pub struct XConf {
        /// Root window size
        pub size: WindowSize,
        /// X11 Depth of the root window
        pub depth: u32,
        /// Memory (in KiB) required by the root window, with at least 1 byte to spare
        pub mem: u32,
    }

    /// Daemon ⇒ agent: Version and root window configuration; sent only at
    /// startup, without a header.  Only used in protocol 1.4 and better.
    pub struct XConfVersion {
        /// Negotiated protocol version
        pub version: u32,
        /// Root window configuration
        pub xconf: XConf,
    }

    /// Bidirectional: Metadata about a mapping
    pub struct MapInfo {
        /// The window that this is `transient_for`, or 0 if there is no such
        /// window.  The semantics of `transient_for` are defined in the X11
        /// ICCCM (Inter-Client Communication Conventions Manual).
        pub transient_for: u32,
        /// If this is 1, then this window (usually a menu) should not be
        /// managed by the window manager.  If this is 0, the window should be
        /// managed by the window manager.  All other values are invalid.  The
        /// semantics of this flag are the same as the X11 override_redirect
        /// flag, which this is implemented in terms of.
        pub override_redirect: u32,
    }

    /// Agent ⇒ daemon: Create a window.  This should always be followed by a
    /// [`Configure`] message.  The window is not immediately mapped.
    pub struct Create {
        /// Rectangle the window is to occupy.  It is a protocol error for the
        /// width or height to be zero, for the width to exceed
        /// [`MAX_WINDOW_WIDTH`], or for the height to exceed [`MAX_WINDOW_HEIGHT`].
        pub rectangle: Rectangle,
        /// Parent window, or [`None`] if there is no parent window.  It is a
        /// protocol error to specify a parent window that does not exist.  The
        /// parent window (or lack theirof) cannot be changed after a window has
        /// been created.
        pub parent: Option<NonZeroU32>,
        /// If this is 1, then this window (usually a menu) should not be
        /// managed by the window manager.  If this is 0, the window should be
        /// managed by the window manager.  All other values are invalid.
        pub override_redirect: u32,
    }

    /// Daemon ⇒ agent: Keypress
    pub struct Keypress {
        /// The X11 type of key pressed.  MUST be 2 ([`EV_KEY_PRESS`]) or 3
        /// ([`EV_KEY_RELEASE`]).  Anything else is a protocol violation.
        pub ty: u32,
        /// Coordinates of the key press
        pub coordinates: Coordinates,
        /// X11 key press state
        pub state: u32,
        /// X11 key code
        pub keycode: u32,
    }

    /// Daemon ⇒ agent: Button press
    pub struct Button {
        /// The type of event.  MUST be 4 ([`EV_BUTTON_PRESS`]) or 5
        /// ([`EV_BUTTON_RELEASE`]).  Anything else is a protocol violation.
        pub ty: u32,
        /// Coordinates of the button press
        pub coordinates: Coordinates,
        /// Bitmask of modifier keys
        pub state: u32,
        /// X11 button number
        pub button: u32,
    }

    /// Daemon ⇒ agent: Motion event
    pub struct Motion {
        /// Coordinates of the motion event
        pub coordinates: Coordinates,
        /// Bitmask of buttons that are pressed
        pub state: u32,
        /// X11 is_hint flag
        pub is_hint: u32,
    }

    /// Daemon ⇒ agent: Crossing event
    pub struct Crossing {
        /// Type of the crossing
        pub ty: u32,
        /// Coordinates of the crossing
        pub coordinates: Coordinates,
        /// X11 state of the crossing
        pub state: u32,
        /// X11 mode of the crossing
        pub mode: u32,
        /// X11 detail of the crossing
        pub detail: u32,
        /// X11 focus of the crossing
        pub focus: u32,
    }

    /// Bidirectional: Configure event
    pub struct Configure {
        /// Desired rectangle position and size
        pub rectangle: Rectangle,
        /// If this is 1, then this window (usually a menu) should not be
        /// managed by the window manager.  If this is 0, the window should be
        /// managed by the window manager.  All other values are invalid.
        pub override_redirect: u32,
    }

    /// Agent ⇒ daemon: Update the given region of the window from the contents of shared memory
    pub struct ShmImage {
        /// Rectangle to update
        pub rectangle: Rectangle,
    }

    /// Daemon ⇒ agent: Focus event from GUI qube
    pub struct Focus {
        /// The type of event.  MUST be 9 ([`EV_FOCUS_IN`]) or 10
        /// ([`EV_FOCUS_OUT`]).  Anything else is a protocol error.
        pub ty: u32,
        /// The X11 event mode.  This is not used in the Qubes GUI protocol.
        /// Daemons MUST set this to 0 to avoid information leaks.  Agents MAY
        /// consider nonzero values to be a protocol error.
        pub mode: u32,
        /// The X11 event detail.  MUST be between 0 and 7 inclusive.
        pub detail: u32,
    }

    /// Agent ⇒ daemon: Set the window name
    pub struct WMName {
        /// NUL-terminated name
        pub data: [u8; 128],
    }

    /// Agent ⇒ daemon: Unmap the window.  Unmapping a window that is not
    /// currently mapped has no effect.
    pub struct Unmap {}

    /// Agent ⇒ daemon: Dock the window.  Docking an already-docked window has
    /// no effect.
    pub struct Dock {}

    /// Agent ⇒ daemon: Destroy the window.  The agent SHOULD NOT reuse the
    /// window ID for as long as possible to make races less likely.
    pub struct Destroy {}

    /// Daemon ⇒ agent: Keymap change notification
    pub struct KeymapNotify {
        /// X11 keymap returned by XQueryKeymap()
        pub keys: [u8; 32],
    }

    /// Agent ⇒ daemon: Set window hints
    pub struct WindowHints {
        /// Which elements are valid?
        pub flags: u32,
        /// Minimum size
        pub min_size: WindowSize,
        /// Maximum size
        pub max_size: WindowSize,
        /// Size increment
        pub size_increment: WindowSize,
        /// Base size
        pub size_base: WindowSize,
    }

    /// Bidirectional: Set window flags
    pub struct WindowFlags {
        /// Flags to set
        pub set: u32,
        /// Flags to unset
        pub unset: u32,
    }

    /// Agent ⇒ daemon: map mfns, deprecated
    pub struct ShmCmd {
        /// ID of the shared memory segment.  Unused; SHOULD be 0.
        pub shmid: u32,
        /// Width of the rectangle to update
        pub width: u32,
        /// Height of the rectangle to update
        pub height: u32,
        /// Bits per pixel; MUST be 24
        pub bpp: u32,
        /// Offset from first page.  MUST be less than 4096.
        pub off: u32,
        /// Number of pages to map.  These follow this struct.
        pub num_mfn: u32,
        /// Source domain ID.  Unused; SHOULD be 0.
        pub domid: u32,
    }

    /// Agent ⇒ daemon: set window class
    pub struct WMClass {
        /// Window class
        pub res_class: [u8; 64],
        /// Window name
        pub res_name: [u8; 64],
    }

    /// Agent ⇒ daemon: Header of a window dump message
    pub struct WindowDumpHeader {
        /// Type of message
        pub ty: u32,
        /// Width in pixels
        pub width: u32,
        /// Height in pixels
        pub height: u32,
        /// Bits per pixel.  MUST be 24.
        pub bpp: u32,
    }

    /// Agent ⇒ daemon: Header of a window dump message
    pub struct Cursor {
        /// Type of cursor
        pub cursor: u32,
    }

    /// Daemon ⇒ agent: Acknowledge a window dump message
    pub struct DumpAck {}
}

macro_rules! impl_message {
    ($(($t: ty, $kind: expr),)+) => {
        $(impl Message for $t {
            const KIND: Msg = $kind;
        })+
    }
}

impl_message! {
    (MapInfo, Msg::Map),
    (Create, Msg::Create),
    (Keypress, Msg::Keypress),
    (Button, Msg::Button),
    (Motion, Msg::Motion),
    (Crossing, Msg::Crossing),
    (Configure, Msg::Configure),
    (ShmImage, Msg::ShmImage),
    (Focus, Msg::Focus),
    (WMName, Msg::SetTitle),
    (KeymapNotify, Msg::KeymapNotify),
    (WindowHints, Msg::WindowHints),
    (WindowFlags, Msg::WindowFlags),
    (ShmCmd, Msg::ShmImage),
    (WMClass, Msg::WindowClass),
    (WindowDumpHeader, Msg::WindowDump),
    (Cursor, Msg::Cursor),
    (Destroy, Msg::Destroy),
    (Dock, Msg::Dock),
    (Unmap, Msg::Unmap),
}

/// Error indicating that the length of a message is bad
#[derive(Debug)]
pub struct BadLengthError {
    /// The type of the bad message
    pub ty: u32,
    /// The length of the bad message
    pub untrusted_len: u32,
}

impl core::fmt::Display for BadLengthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Bad length {} for message of type {}",
            self.untrusted_len, self.ty
        )
    }
}

/// A header that has been validated to be a valid message.
///
/// Transmuting a [`Header`] to an [`UntrustedHeader`] is safe.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(transparent)]
pub struct Header(UntrustedHeader);

impl Header {
    /// Get the type of the header as a u32.
    ///
    /// The type is guaranteed to be a valid message type.
    pub fn ty(&self) -> u32 {
        self.0.ty
    }

    /// Get the window ID of the header.  This has not been validated.
    pub fn untrusted_window(&self) -> WindowID {
        self.0.window
    }

    /// Get the length of the object represented by the Header.
    ///
    /// It is safe to use this length to e.g. allocate a buffer.
    ///
    /// The return value is guaranteed to be a valid length for the given
    /// message type.
    pub fn len(&self) -> usize {
        self.0.untrusted_len as usize
    }

    /// Obtain the inner [`UntrustedHeader`].  Calling [`UntrustedHeader::validate_length`] on the
    /// return value is guaranteed to return `Ok(Some)`.
    pub fn inner(&self) -> UntrustedHeader {
        self.0
    }
}

impl UntrustedHeader {
    /// Validate that the length of this header is correct
    ///
    /// # Returns
    ///
    /// If the message is good, returns a [`Header`] wrapped in `Ok(Some())`.
    /// If the message is unknown, returns Ok(None).
    ///
    /// # Errors
    ///
    /// Returns an error if the length is bad, or if the type of the message is
    /// not valid in any supported protocol version.
    pub fn validate_length(&self) -> Result<Option<Header>, BadLengthError> {
        const U32_SIZE: u32 = size_of::<u32>() as u32;
        use core::mem::size_of;
        let untrusted_len = self.untrusted_len;
        if match self.ty {
            MSG_CLIPBOARD_DATA => untrusted_len <= MAX_CLIPBOARD_SIZE,
            MSG_BUTTON => untrusted_len == size_of::<Button>() as u32,
            MSG_KEYPRESS => untrusted_len == size_of::<Keypress>() as u32,
            MSG_MOTION => untrusted_len == size_of::<Motion>() as u32,
            MSG_CROSSING => untrusted_len == size_of::<Crossing>() as u32,
            MSG_FOCUS => untrusted_len == size_of::<Focus>() as u32,
            MSG_CREATE => untrusted_len == size_of::<Create>() as u32,
            MSG_DESTROY => untrusted_len == 0,
            MSG_MAP => untrusted_len == size_of::<MapInfo>() as u32,
            MSG_UNMAP => untrusted_len == 0,
            MSG_CONFIGURE => untrusted_len == size_of::<Configure>() as u32,
            MSG_MFNDUMP if untrusted_len % U32_SIZE != 0 => false,
            MSG_MFNDUMP => untrusted_len / U32_SIZE <= MAX_MFN_COUNT,
            MSG_SHMIMAGE => untrusted_len == size_of::<ShmImage>() as u32,
            MSG_CLOSE | MSG_CLIPBOARD_REQ => untrusted_len == 0,
            MSG_SET_TITLE => untrusted_len == size_of::<WMName>() as u32,
            MSG_KEYMAP_NOTIFY => untrusted_len == size_of::<KeymapNotify>() as u32,
            MSG_DOCK => untrusted_len == 0,
            MSG_WINDOW_HINTS => untrusted_len == size_of::<WindowHints>() as u32,
            MSG_WINDOW_FLAGS => untrusted_len == size_of::<WindowFlags>() as u32,
            MSG_WINDOW_CLASS => untrusted_len == size_of::<WMClass>() as u32,
            MSG_WINDOW_DUMP if untrusted_len < size_of::<WindowDumpHeader>() as u32 => false,
            MSG_WINDOW_DUMP => {
                let refs_len = untrusted_len - size_of::<WindowDumpHeader>() as u32;
                (refs_len % U32_SIZE) == 0 && (refs_len / U32_SIZE) <= MAX_GRANT_REFS_COUNT
            }
            MSG_CURSOR => untrusted_len == size_of::<Cursor>() as u32,
            MSG_WINDOW_DUMP_ACK => untrusted_len == 0,
            MSG_EXECUTE => false,
            _ => return Ok(None),
        } {
            Ok(Some(Header(*self)))
        } else {
            Err(BadLengthError {
                ty: self.ty,
                untrusted_len: self.untrusted_len,
            })
        }
    }
}
