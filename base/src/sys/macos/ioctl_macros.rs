// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS ioctl number encoding — BSD-style, same as Linux but different constant names.
// On macOS/BSD: _IOC(dir, type, nr, size) = (dir<<29 | size<<16 | type<<8 | nr)
// This matches Linux's encoding for arm64.

#![allow(dead_code)]

pub const _IOC_NRBITS: u32 = 8;
pub const _IOC_TYPEBITS: u32 = 8;
pub const _IOC_SIZEBITS: u32 = 13; // macOS uses 13 bits for size
pub const _IOC_DIRBITS: u32 = 3;   // macOS uses 3 bits for direction

pub const _IOC_NRSHIFT: u32 = 0;
pub const _IOC_TYPESHIFT: u32 = _IOC_NRSHIFT + _IOC_NRBITS;
pub const _IOC_SIZESHIFT: u32 = _IOC_TYPESHIFT + _IOC_TYPEBITS;
pub const _IOC_DIRSHIFT: u32 = _IOC_SIZESHIFT + _IOC_SIZEBITS;

pub const _IOC_NONE: u32 = 0;
pub const _IOC_WRITE: u32 = 1;
pub const _IOC_READ: u32 = 2;

/// Raw macro to declare the expression that calculates an ioctl number (macOS version).
#[macro_export]
macro_rules! ioctl_expr {
    ($dir:expr, $ty:expr, $nr:expr, $size:expr) => {
        ((($dir as $crate::macos::IoctlNr) << $crate::macos::ioctl_macros::_IOC_DIRSHIFT)
            | (($ty as $crate::macos::IoctlNr) << $crate::macos::ioctl_macros::_IOC_TYPESHIFT)
            | (($nr as $crate::macos::IoctlNr) << $crate::macos::ioctl_macros::_IOC_NRSHIFT)
            | (($size as $crate::macos::IoctlNr) << $crate::macos::ioctl_macros::_IOC_SIZESHIFT))
    };
}

#[macro_export]
macro_rules! ioctl_ioc_nr {
    ($name:ident, $dir:expr, $ty:expr, $nr:expr, $size:expr) => {
        #[allow(non_snake_case)]
        pub const $name: $crate::macos::IoctlNr = $crate::ioctl_expr!($dir, $ty, $nr, $size);
    };
    ($name:ident, $dir:expr, $ty:expr, $nr:expr, $size:expr, $($v:ident),+) => {
        #[allow(non_snake_case)]
        pub const fn $name($($v: ::std::os::raw::c_uint),+) -> $crate::macos::IoctlNr {
            $crate::ioctl_expr!($dir, $ty, $nr + $($v as u32)+, $size)
        }
    };
}

#[macro_export]
macro_rules! ioctl_io_nr {
    ($name:ident, $ty:expr, $nr:expr) => {
        $crate::ioctl_ioc_nr!($name, $crate::macos::ioctl_macros::_IOC_NONE, $ty, $nr, 0);
    };
}

#[macro_export]
macro_rules! ioctl_ior_nr {
    ($name:ident, $ty:expr, $nr:expr, $size:ty) => {
        $crate::ioctl_ioc_nr!(
            $name,
            $crate::macos::ioctl_macros::_IOC_READ,
            $ty,
            $nr,
            ::std::mem::size_of::<$size>() as u32
        );
    };
}

#[macro_export]
macro_rules! ioctl_iow_nr {
    ($name:ident, $ty:expr, $nr:expr, $size:ty) => {
        $crate::ioctl_ioc_nr!(
            $name,
            $crate::macos::ioctl_macros::_IOC_WRITE,
            $ty,
            $nr,
            ::std::mem::size_of::<$size>() as u32
        );
    };
}

#[macro_export]
macro_rules! ioctl_iowr_nr {
    ($name:ident, $ty:expr, $nr:expr, $size:ty) => {
        $crate::ioctl_ioc_nr!(
            $name,
            $crate::macos::ioctl_macros::_IOC_READ | $crate::macos::ioctl_macros::_IOC_WRITE,
            $ty,
            $nr,
            ::std::mem::size_of::<$size>() as u32
        );
    };
}
