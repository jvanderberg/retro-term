//! ZMODEM protocol constants (names follow Chuck Forsberg's `zmodem.h`).

// Framing characters.
pub const ZPAD: u8 = b'*';
pub const ZDLE: u8 = 0x18;
pub const CAN: u8 = 0x18;
pub const BS: u8 = 0x08;
pub const XON: u8 = 0x11;
pub const XOFF: u8 = 0x13;

// Header formats (the byte after `ZPAD ZDLE`).
pub const ZBIN: u8 = b'A';
pub const ZHEX: u8 = b'B';
pub const ZBIN32: u8 = b'C';

// Frame types.
pub const ZRQINIT: u8 = 0;
pub const ZRINIT: u8 = 1;
pub const ZSINIT: u8 = 2;
pub const ZACK: u8 = 3;
pub const ZFILE: u8 = 4;
pub const ZSKIP: u8 = 5;
pub const ZNAK: u8 = 6;
pub const ZABORT: u8 = 7;
pub const ZFIN: u8 = 8;
pub const ZRPOS: u8 = 9;
pub const ZDATA: u8 = 10;
pub const ZEOF: u8 = 11;
pub const ZCRC: u8 = 13;
pub const ZCHALLENGE: u8 = 14;
pub const ZCOMPL: u8 = 15;
pub const ZCAN: u8 = 16;
pub const ZFREECNT: u8 = 17;
pub const ZCOMMAND: u8 = 18;

// Data subpacket terminators (sent as `ZDLE x`).
pub const ZCRCE: u8 = b'h';
pub const ZCRCG: u8 = b'i';
pub const ZCRCQ: u8 = b'j';
pub const ZCRCW: u8 = b'k';
pub const ZRUB0: u8 = b'l';
pub const ZRUB1: u8 = b'm';

// ZRINIT ZF0 capability flags.
pub const CANFDX: u8 = 0x01;
pub const CANOVIO: u8 = 0x02;
pub const CANFC32: u8 = 0x20;
pub const ESCCTL: u8 = 0x40;

// ZSINIT ZF0 flags.
pub const TESCCTL: u8 = 0x40;

// ZFILE ZF0 conversion option: binary.
pub const ZCBIN: u8 = 1;
